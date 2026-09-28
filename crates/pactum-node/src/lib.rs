//! Off-chain agents for Pactum: the maker that signs intents and releases
//! secrets, and the resolver that competes in the Dutch auction to fill them.

pub mod chain;
pub mod contracts;
pub mod filler;
pub mod maker;

pub use chain::{Chain, ChainConfig, NATIVE};
pub use filler::{Bid, IntentFiller, Job, JobStatus};
pub use maker::{IntentRequest, Maker, OpenIntent};
