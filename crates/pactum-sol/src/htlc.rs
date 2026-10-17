//! Client side of `programs/pactum-solana`: instruction builders and the
//! escrow account layout. Kept byte-compatible with the program.

use anyhow::{Context, ensure};

use crate::{
    rpc::SolanaRpc,
    tx::{AccountMeta, Instruction, Keypair, Pubkey, SYSTEM_PROGRAM, find_program_address},
};

pub const ESCROW_LEN: usize = 32 + 32 + 32 + 8 + 8 + 1 + 1 + 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscrowState {
    Open,
    Redeemed,
    Refunded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Escrow {
    pub initiator: Pubkey,
    pub redeemer: Pubkey,
    pub secret_hash: [u8; 32],
    pub amount: u64,
    pub expiry: i64,
    pub state: EscrowState,
    pub secret: Option<[u8; 32]>,
}

impl Escrow {
    pub fn decode(data: &[u8]) -> anyhow::Result<Self> {
        ensure!(data.len() == ESCROW_LEN, "escrow is {} bytes", data.len());
        let k = |at: usize| Pubkey(data[at..at + 32].try_into().expect("32"));
        // initiator 0..32 | redeemer 32..64 | hash 64..96 | amount 96..104 |
        // expiry 104..112 | state 112 | bump 113 | secret 114..146
        let state = match data[112] {
            0 => EscrowState::Open,
            1 => EscrowState::Redeemed,
            2 => EscrowState::Refunded,
            s => anyhow::bail!("unknown escrow state {s}"),
        };
        let secret: [u8; 32] = data[114..146].try_into().expect("32");
        Ok(Escrow {
            initiator: k(0),
            redeemer: k(32),
            secret_hash: data[64..96].try_into().expect("32"),
            amount: u64::from_le_bytes(data[96..104].try_into().expect("8")),
            expiry: i64::from_le_bytes(data[104..112].try_into().expect("8")),
            state,
            secret: (state == EscrowState::Redeemed).then_some(secret),
        })
    }
}

/// A deployed Pactum HTLC program.
#[derive(Clone)]
pub struct HtlcProgram {
    pub id: Pubkey,
    pub rpc: SolanaRpc,
}

impl HtlcProgram {
    pub fn escrow_address(&self, swap_id: &[u8; 32]) -> Pubkey {
        find_program_address(&[b"escrow", swap_id], &self.id).0
    }

    pub async fn escrow(&self, swap_id: &[u8; 32]) -> anyhow::Result<Option<Escrow>> {
        let Some((data, owner, _)) = self.rpc.account(&self.escrow_address(swap_id)).await? else {
            return Ok(None);
        };
        ensure!(owner == self.id, "escrow not owned by the program");
        if data.iter().all(|b| *b == 0) {
            return Ok(None); // closed
        }
        Escrow::decode(&data).map(Some)
    }

    pub async fn initiate(
        &self,
        initiator: &Keypair,
        swap_id: [u8; 32],
        redeemer: Pubkey,
        secret_hash: [u8; 32],
        lamports: u64,
        expiry: i64,
    ) -> anyhow::Result<String> {
        let mut data = vec![0u8];
        data.extend_from_slice(&swap_id);
        data.extend_from_slice(&redeemer.0);
        data.extend_from_slice(&secret_hash);
        data.extend_from_slice(&lamports.to_le_bytes());
        data.extend_from_slice(&expiry.to_le_bytes());
        let ix = Instruction {
            program_id: self.id,
            accounts: vec![
                AccountMeta::new(initiator.pubkey(), true, true),
                AccountMeta::new(self.escrow_address(&swap_id), false, true),
                AccountMeta::new(SYSTEM_PROGRAM, false, false),
            ],
            data,
        };
        self.rpc.send(&[ix], initiator, &[]).await
    }

    /// Permissionless: `fee_payer` submits, the escrow's redeemer is paid.
    pub async fn redeem(
        &self,
        fee_payer: &Keypair,
        swap_id: [u8; 32],
        redeemer: Pubkey,
        secret: [u8; 32],
    ) -> anyhow::Result<String> {
        let ix = Instruction {
            program_id: self.id,
            accounts: vec![
                AccountMeta::new(self.escrow_address(&swap_id), false, true),
                AccountMeta::new(redeemer, false, true),
            ],
            data: [&[1u8][..], &secret].concat(),
        };
        self.rpc.send(&[ix], fee_payer, &[]).await
    }

    /// Permissionless after expiry: the escrow's initiator is refunded.
    pub async fn refund(
        &self,
        fee_payer: &Keypair,
        swap_id: [u8; 32],
        initiator: Pubkey,
    ) -> anyhow::Result<String> {
        let ix = Instruction {
            program_id: self.id,
            accounts: vec![
                AccountMeta::new(self.escrow_address(&swap_id), false, true),
                AccountMeta::new(initiator, false, true),
            ],
            data: vec![2],
        };
        self.rpc.send(&[ix], fee_payer, &[]).await
    }

    /// Reclaim a settled escrow's rent.
    pub async fn close(&self, initiator: &Keypair, swap_id: [u8; 32]) -> anyhow::Result<String> {
        let ix = Instruction {
            program_id: self.id,
            accounts: vec![
                AccountMeta::new(self.escrow_address(&swap_id), false, true),
                AccountMeta::new(initiator.pubkey(), true, true),
            ],
            data: vec![3],
        };
        self.rpc.send(&[ix], initiator, &[]).await.context("close")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_program_layout() {
        let mut d = vec![0u8; ESCROW_LEN];
        d[0..32].fill(1);
        d[32..64].fill(2);
        d[64..96].fill(3);
        d[96..104].copy_from_slice(&42u64.to_le_bytes());
        d[104..112].copy_from_slice(&1_900_000_000i64.to_le_bytes());
        d[112] = 1;
        d[113] = 254;
        d[114..146].fill(9);
        let e = Escrow::decode(&d).unwrap();
        assert_eq!(e.initiator, Pubkey([1; 32]));
        assert_eq!(e.redeemer, Pubkey([2; 32]));
        assert_eq!(e.amount, 42);
        assert_eq!(e.expiry, 1_900_000_000);
        assert_eq!(e.state, EscrowState::Redeemed);
        assert_eq!(e.secret, Some([9; 32]));
    }
}
