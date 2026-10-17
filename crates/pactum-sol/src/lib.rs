//! Minimal Solana client for the Pactum HTLC program: keys, PDAs, legacy
//! transaction encoding, JSON-RPC, and the program's instructions and account
//! layout. Written against the wire format so the workspace stays free of the
//! Solana SDK; the program itself lives in `programs/pactum-solana`.

pub mod htlc;
pub mod rpc;
pub mod tx;

pub use htlc::{Escrow, EscrowState, HtlcProgram};
pub use rpc::SolanaRpc;
pub use tx::{Keypair, Pubkey};
