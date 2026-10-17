//! Pactum HTLC program for Solana.
//!
//! One escrow account (a PDA at `["escrow", swap_id]`) per swap holds native
//! SOL until either:
//! - anyone submits the 32-byte secret whose `sha256` is the escrow's hash,
//!   which pays the fixed `redeemer` (so a third party, such as a resolver,
//!   can settle on the redeemer's behalf and the redeemer pays no fees), or
//! - the Unix-time `expiry` passes, after which anyone can refund the initiator.
//!
//! The revealed secret is stored in the escrow, so watchers on other chains
//! read it straight from account data. The initiator can close a settled
//! escrow to reclaim its rent.
//!
//! Instruction data is a one-byte tag followed by fixed-width little-endian
//! fields; see [`Instruction`].

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    hash::hash as sha256,
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    system_instruction,
    sysvar::{clock::Clock, rent::Rent, Sysvar},
};

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

pub const ESCROW_SEED: &[u8] = b"escrow";
pub const SECRET_LEN: usize = 32;

/// Escrow states.
pub const OPEN: u8 = 0;
pub const REDEEMED: u8 = 1;
pub const REFUNDED: u8 = 2;

/// `initiator | redeemer | secret_hash | amount | expiry | state | bump | secret`
pub const ESCROW_LEN: usize = 32 + 32 + 32 + 8 + 8 + 1 + 1 + 32;

#[derive(Debug, PartialEq, Eq)]
pub enum Instruction {
    /// Lock `amount` lamports from the signer for `redeemer`.
    /// Accounts: `[signer, writable] initiator`, `[writable] escrow`, `[] system_program`.
    Initiate { swap_id: [u8; 32], redeemer: Pubkey, secret_hash: [u8; 32], amount: u64, expiry: i64 },
    /// Pay the redeemer by revealing the secret.
    /// Accounts: `[writable] escrow`, `[writable] redeemer`.
    Redeem { secret: [u8; 32] },
    /// Return the funds to the initiator after expiry.
    /// Accounts: `[writable] escrow`, `[writable] initiator`.
    Refund,
    /// Reclaim the rent of a settled escrow.
    /// Accounts: `[writable] escrow`, `[signer, writable] initiator`.
    Close,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Escrow {
    pub initiator: Pubkey,
    pub redeemer: Pubkey,
    pub secret_hash: [u8; 32],
    pub amount: u64,
    pub expiry: i64,
    pub state: u8,
    pub bump: u8,
    pub secret: [u8; 32],
}

fn take<const N: usize>(data: &[u8], at: &mut usize) -> Result<[u8; N], ProgramError> {
    let bytes = data.get(*at..*at + N).ok_or(ProgramError::InvalidInstructionData)?;
    *at += N;
    Ok(bytes.try_into().expect("length checked"))
}

impl Instruction {
    pub fn unpack(data: &[u8]) -> Result<Self, ProgramError> {
        let (&tag, rest) = data.split_first().ok_or(ProgramError::InvalidInstructionData)?;
        let mut at = 0;
        let ix = match tag {
            0 => Instruction::Initiate {
                swap_id: take(rest, &mut at)?,
                redeemer: Pubkey::new_from_array(take(rest, &mut at)?),
                secret_hash: take(rest, &mut at)?,
                amount: u64::from_le_bytes(take(rest, &mut at)?),
                expiry: i64::from_le_bytes(take(rest, &mut at)?),
            },
            1 => Instruction::Redeem { secret: take(rest, &mut at)? },
            2 => Instruction::Refund,
            3 => Instruction::Close,
            _ => return Err(ProgramError::InvalidInstructionData),
        };
        if at != rest.len() {
            return Err(ProgramError::InvalidInstructionData);
        }
        Ok(ix)
    }

    pub fn pack(&self) -> Vec<u8> {
        match self {
            Instruction::Initiate { swap_id, redeemer, secret_hash, amount, expiry } => {
                let mut v = vec![0];
                v.extend_from_slice(swap_id);
                v.extend_from_slice(redeemer.as_ref());
                v.extend_from_slice(secret_hash);
                v.extend_from_slice(&amount.to_le_bytes());
                v.extend_from_slice(&expiry.to_le_bytes());
                v
            }
            Instruction::Redeem { secret } => [&[1u8][..], secret].concat(),
            Instruction::Refund => vec![2],
            Instruction::Close => vec![3],
        }
    }
}

impl Escrow {
    pub fn unpack(data: &[u8]) -> Result<Self, ProgramError> {
        if data.len() != ESCROW_LEN {
            return Err(ProgramError::InvalidAccountData);
        }
        let mut at = 0;
        Ok(Escrow {
            initiator: Pubkey::new_from_array(take(data, &mut at)?),
            redeemer: Pubkey::new_from_array(take(data, &mut at)?),
            secret_hash: take(data, &mut at)?,
            amount: u64::from_le_bytes(take(data, &mut at)?),
            expiry: i64::from_le_bytes(take(data, &mut at)?),
            state: take::<1>(data, &mut at)?[0],
            bump: take::<1>(data, &mut at)?[0],
            secret: take(data, &mut at)?,
        })
    }

    pub fn pack(&self, out: &mut [u8]) {
        let mut v = Vec::with_capacity(ESCROW_LEN);
        v.extend_from_slice(self.initiator.as_ref());
        v.extend_from_slice(self.redeemer.as_ref());
        v.extend_from_slice(&self.secret_hash);
        v.extend_from_slice(&self.amount.to_le_bytes());
        v.extend_from_slice(&self.expiry.to_le_bytes());
        v.push(self.state);
        v.push(self.bump);
        v.extend_from_slice(&self.secret);
        out.copy_from_slice(&v);
    }
}

pub fn escrow_address(program_id: &Pubkey, swap_id: &[u8; 32]) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[ESCROW_SEED, swap_id], program_id)
}

pub fn process_instruction(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    match Instruction::unpack(data)? {
        Instruction::Initiate { swap_id, redeemer, secret_hash, amount, expiry } => {
            initiate(program_id, accounts, swap_id, redeemer, secret_hash, amount, expiry)
        }
        Instruction::Redeem { secret } => redeem(program_id, accounts, secret),
        Instruction::Refund => refund(program_id, accounts),
        Instruction::Close => close(program_id, accounts),
    }
}

fn initiate(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    swap_id: [u8; 32],
    redeemer: Pubkey,
    secret_hash: [u8; 32],
    amount: u64,
    expiry: i64,
) -> ProgramResult {
    let it = &mut accounts.iter();
    let initiator = next_account_info(it)?;
    let escrow = next_account_info(it)?;
    let system = next_account_info(it)?;

    if !initiator.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let (expected, bump) = escrow_address(program_id, &swap_id);
    if *escrow.key != expected {
        return Err(ProgramError::InvalidSeeds);
    }
    if escrow.lamports() != 0 || !escrow.data_is_empty() {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    if amount == 0 || redeemer == Pubkey::default() {
        return Err(ProgramError::InvalidArgument);
    }
    if expiry <= Clock::get()?.unix_timestamp {
        return Err(ProgramError::InvalidArgument);
    }

    let lamports = Rent::get()?
        .minimum_balance(ESCROW_LEN)
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    invoke_signed(
        &system_instruction::create_account(
            initiator.key,
            escrow.key,
            lamports,
            ESCROW_LEN as u64,
            program_id,
        ),
        &[initiator.clone(), escrow.clone(), system.clone()],
        &[&[ESCROW_SEED, &swap_id, &[bump]]],
    )?;

    Escrow {
        initiator: *initiator.key,
        redeemer,
        secret_hash,
        amount,
        expiry,
        state: OPEN,
        bump,
        secret: [0; 32],
    }
    .pack(&mut escrow.try_borrow_mut_data()?);
    msg!("pactum: initiated {} lamports", amount);
    Ok(())
}

fn open_escrow(program_id: &Pubkey, escrow: &AccountInfo) -> Result<Escrow, ProgramError> {
    if escrow.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let e = Escrow::unpack(&escrow.try_borrow_data()?)?;
    if e.state != OPEN {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(e)
}

/// Move `amount` lamports out of a program-owned escrow.
fn pay(escrow: &AccountInfo, to: &AccountInfo, amount: u64) -> ProgramResult {
    let from = escrow.lamports().checked_sub(amount).ok_or(ProgramError::InsufficientFunds)?;
    let into = to.lamports().checked_add(amount).ok_or(ProgramError::ArithmeticOverflow)?;
    **escrow.try_borrow_mut_lamports()? = from;
    **to.try_borrow_mut_lamports()? = into;
    Ok(())
}

fn redeem(program_id: &Pubkey, accounts: &[AccountInfo], secret: [u8; 32]) -> ProgramResult {
    let it = &mut accounts.iter();
    let escrow = next_account_info(it)?;
    let redeemer = next_account_info(it)?;

    let mut e = open_escrow(program_id, escrow)?;
    if *redeemer.key != e.redeemer {
        return Err(ProgramError::InvalidArgument);
    }
    // `secret` is 32 bytes by construction, matching OP_SIZE 32 on Bitcoin
    if sha256(&secret).to_bytes() != e.secret_hash {
        msg!("pactum: wrong secret");
        return Err(ProgramError::InvalidArgument);
    }

    e.state = REDEEMED;
    e.secret = secret;
    e.pack(&mut escrow.try_borrow_mut_data()?);
    pay(escrow, redeemer, e.amount)?;
    msg!("pactum: redeemed");
    Ok(())
}

fn refund(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let escrow = next_account_info(it)?;
    let initiator = next_account_info(it)?;

    let mut e = open_escrow(program_id, escrow)?;
    if *initiator.key != e.initiator {
        return Err(ProgramError::InvalidArgument);
    }
    if Clock::get()?.unix_timestamp < e.expiry {
        msg!("pactum: not expired");
        return Err(ProgramError::InvalidArgument);
    }

    e.state = REFUNDED;
    e.pack(&mut escrow.try_borrow_mut_data()?);
    pay(escrow, initiator, e.amount)?;
    msg!("pactum: refunded");
    Ok(())
}

fn close(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let escrow = next_account_info(it)?;
    let initiator = next_account_info(it)?;

    if escrow.owner != program_id {
        return Err(ProgramError::IncorrectProgramId);
    }
    let e = Escrow::unpack(&escrow.try_borrow_data()?)?;
    if !initiator.is_signer || *initiator.key != e.initiator {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if e.state == OPEN {
        return Err(ProgramError::InvalidAccountData);
    }
    let rest = escrow.lamports();
    pay(escrow, initiator, rest)?;
    escrow.try_borrow_mut_data()?.fill(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_round_trip() {
        let ix = Instruction::Initiate {
            swap_id: [1; 32],
            redeemer: Pubkey::new_from_array([2; 32]),
            secret_hash: [3; 32],
            amount: 42,
            expiry: 1_900_000_000,
        };
        assert_eq!(Instruction::unpack(&ix.pack()).unwrap(), ix);
        let r = Instruction::Redeem { secret: [9; 32] };
        assert_eq!(Instruction::unpack(&r.pack()).unwrap(), r);
        assert_eq!(Instruction::unpack(&[2]).unwrap(), Instruction::Refund);
    }

    #[test]
    fn rejects_malformed_data() {
        assert!(Instruction::unpack(&[]).is_err());
        assert!(Instruction::unpack(&[9]).is_err());
        // 31-byte secret: the program never accepts a short preimage
        assert!(Instruction::unpack(&[&[1u8][..], &[0; 31]].concat()).is_err());
        // trailing bytes
        assert!(Instruction::unpack(&[2, 0]).is_err());
    }

    #[test]
    fn escrow_round_trip() {
        let e = Escrow {
            initiator: Pubkey::new_from_array([1; 32]),
            redeemer: Pubkey::new_from_array([2; 32]),
            secret_hash: [3; 32],
            amount: 7,
            expiry: -1,
            state: REDEEMED,
            bump: 254,
            secret: [4; 32],
        };
        let mut buf = [0u8; ESCROW_LEN];
        e.pack(&mut buf);
        assert_eq!(Escrow::unpack(&buf).unwrap(), e);
    }

    #[test]
    fn sha256_matches_other_chains() {
        // same vector as the Solidity and Bitcoin tests: sha256 of 32 zero bytes
        assert_eq!(
            sha256(&[0u8; 32]).to_bytes()[..4],
            [0x66, 0x68, 0x7a, 0xad]
        );
    }
}
