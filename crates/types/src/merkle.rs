use crate::hash::{Hash32, blake3_concat};


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
