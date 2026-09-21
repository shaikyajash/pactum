//! Protocol types and pure logic shared by the Pactum relay, maker and resolver.
//!
//! Everything here mirrors the Solidity contracts byte-for-byte: EIP-712 order
//! hashing, the Merkle commitment to secrets, auction pricing, secret-bucket
//! selection and swap ids. Nothing in this crate does I/O.

pub mod auction;
pub mod merkle;
pub mod order;
pub mod secret;
pub mod swap;
pub mod timelock;

pub use merkle::SecretTree;
pub use order::{Order, OrderSecrets};
pub use secret::Secret;
