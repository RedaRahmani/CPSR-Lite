use crate::hash::{Hash32, blake3_concat, ZERO32};

/// Bottom-up Merkle root; duplicates the last leaf on odd layers (Bitcoin-style).
pub fn merkle_root(mut leaves: Vec<Hash32>) -> Hash32 {
    if leaves.is_empty() {
        return ZERO32;
    }
    while leaves.len() > 1 {
        // duplicate last leaf if odd count to keep a full binary layer
        if leaves.len() % 2 == 1 {
            let last = *leaves.last().unwrap();
            leaves.push(last);
        }
        let mut next = Vec::with_capacity(leaves.len() / 2);
        for pair in leaves.chunks_exact(2) {
            next.push(blake3_concat(&[&pair[0], &pair[1]]));
        }
        leaves = next;
    }
    leaves[0]
}
