use alloy::primitives::{B256, Bytes};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A 32-byte HTLC preimage and its `sha256` hash, as checked by `HashLock.redeem`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secret {
    pub secret: Bytes,
    pub hash: B256,
}

impl Secret {
    pub fn random() -> Self {
        Self::from_bytes(B256::random().to_vec())
    }

    pub fn from_bytes(secret: impl Into<Bytes>) -> Self {
        let secret = secret.into();
        let hash = sha256(&secret);
        Self { secret, hash }
    }
}

pub fn sha256(data: &[u8]) -> B256 {
    B256::from_slice(&Sha256::digest(data))
}

pub fn matches(secret: &[u8], hash: B256) -> bool {
    sha256(secret) == hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::b256;

    #[test]
    fn hash_matches_contract() {
        // sha256 of 32 zero bytes, same vector as the contract tests
        assert_eq!(
            sha256(&[0u8; 32]),
            b256!("66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925")
        );
    }

    #[test]
    fn random_secrets_verify() {
        let s = Secret::random();
        assert_eq!(s.secret.len(), 32);
        assert!(matches(&s.secret, s.hash));
        assert!(!matches(&Secret::random().secret, s.hash));
    }
}
