//! Bitcoin leg for Pactum: a Taproot HTLC that locks BTC behind the same
//! `sha256` secret as the EVM and Solana legs, plus a minimal Bitcoin Core
//! RPC client for funding, watching and settling it.

pub mod htlc;
pub mod rpc;

pub use htlc::{Htlc, HtlcError};
pub use rpc::{BitcoinRpc, Utxo};
