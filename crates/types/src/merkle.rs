use crate::hash::{Hash32, blake3_concat};
// A Merkle tree is a way to combine many fingerprints into one single “master fingerprint” (root) (no bells, no proofs yet).
/// So we can prove later that a specific transaction was inside a batch without storing them all.
// Links: Chunk → merkle_root(intents/leaves); Bundle → merkle_root(chunk_roots).
// Every chunk has a Merkle root, and the whole bundle has a Merkle root of chunks.


pub fn merkle_root(mut leaves: Vec<Hash32>) -> Hash32 {
    if leaves.is_empty() {
        return [0u8; 32];
    }
    while leaves.len() > 1 {
        let mut next = Vec::with_capacity((leaves.len() + 1) / 2);
        for pair in leaves.chunks(2) {
            let h = if pair.len() == 2 {
                blake3_concat(&[&pair[0], &pair[1]])
            } else {
                pair[0]
            };
            next.push(h);
        }
        leaves = next;
    }
    leaves[0]
}
