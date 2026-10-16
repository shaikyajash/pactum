//! Merkle commitment to an ordered list of secret hashes. Mirrors
//! `MerkleSecrets.sol`: leaf = keccak(keccak(abi.encode(index, hash))),
//! sorted-pair hashing, and the last node pairs with itself on odd levels.

use alloy::primitives::{B256, U256, keccak256};
use alloy::sol_types::SolValue;

pub fn leaf(index: usize, secret_hash: B256) -> B256 {
    keccak256(keccak256((U256::from(index), secret_hash).abi_encode()))
}

fn hash_pair(a: B256, b: B256) -> B256 {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    keccak256((lo, hi).abi_encode())
}

#[derive(Clone, Debug)]
pub struct SecretTree {
    hashes: Vec<B256>,
    levels: Vec<Vec<B256>>,
}

impl SecretTree {
    pub fn new(hashes: Vec<B256>) -> Self {
        assert!(!hashes.is_empty(), "empty secret tree");
        let mut level: Vec<B256> = hashes.iter().enumerate().map(|(i, h)| leaf(i, *h)).collect();
        let mut levels = vec![level.clone()];
        while level.len() > 1 {
            level = level
                .chunks(2)
                .map(|pair| hash_pair(pair[0], *pair.get(1).unwrap_or(&pair[0])))
                .collect();
            levels.push(level.clone());
        }
        Self { hashes, levels }
    }

    pub fn root(&self) -> B256 {
        self.levels.last().expect("non-empty")[0]
    }

    pub fn hashes(&self) -> &[B256] {
        &self.hashes
    }

    pub fn proof(&self, index: usize) -> Vec<B256> {
        assert!(index < self.hashes.len(), "no leaf {index}");
        let mut idx = index;
        let mut out = Vec::with_capacity(self.levels.len());
        for level in &self.levels[..self.levels.len() - 1] {
            out.push(*level.get(idx ^ 1).unwrap_or(&level[idx]));
            idx >>= 1;
        }
        out
    }

    pub fn verify(root: B256, index: usize, secret_hash: B256, proof: &[B256]) -> bool {
        proof.iter().fold(leaf(index, secret_hash), |n, s| hash_pair(n, *s)) == root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::sha256;
    use alloy::primitives::b256;

    fn h(i: u64) -> B256 {
        sha256(&U256::from(i).abi_encode())
    }

    #[test]
    fn matches_solidity_tree_builder() {
        // vector from TreeBuilder.rootAndProof(sha256(abi.encode(i)) for i < 5, target 4)
        let tree = SecretTree::new((0..5).map(h).collect());
        assert_eq!(
            tree.root(),
            b256!("b0d3301e731129d289da59aefe64d69ae7770197cb269907bd7df1385a330b71")
        );
        assert_eq!(
            tree.proof(4),
            vec![
                b256!("a0a59bae27a6d04030d96fcef0e9ad7d499d01c0d5b2f367fcd8c10e8c10675a"),
                b256!("98f593d59d90c9e4f525a1a2ab9ba97038ca6a4a7dccd7ec6edb50e87ee266f8"),
                b256!("7f5b348b4fc30bb77f54f446bae792a9997cb8a05eddbca1d0acd706cdb0d2c3"),
            ]
        );
    }

    #[test]
    fn every_leaf_verifies() {
        for n in [1u64, 2, 3, 7, 8, 33] {
            let hashes: Vec<_> = (0..n).map(h).collect();
            let tree = SecretTree::new(hashes.clone());
            for (i, x) in hashes.iter().enumerate() {
                assert!(SecretTree::verify(tree.root(), i, *x, &tree.proof(i)), "n={n} i={i}");
            }
        }
    }

    #[test]
    fn index_is_bound() {
        let tree = SecretTree::new(vec![h(0), h(1), h(2)]);
        assert!(!SecretTree::verify(tree.root(), 0, h(1), &tree.proof(1)));
    }
}
