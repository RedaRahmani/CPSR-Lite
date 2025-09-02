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



#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::blake3_concat;

    #[test]
    fn empty_is_zero_root() {
        assert_eq!(merkle_root(vec![]), ZERO32);
    }

    #[test]
    fn single_leaf_is_identity() {
        let l = blake3_concat(&[b"leaf"]);
        assert_eq!(merkle_root(vec![l]), l);
    }

    #[test]
    fn two_leaves_standard_pair() {
        let a = blake3_concat(&[b"a"]);
        let b = blake3_concat(&[b"b"]);
        let expected = blake3_concat(&[&a, &b]);
        assert_eq!(merkle_root(vec![a, b]), expected);
    }

    #[test]
    fn three_leaves_duplicate_last() {
        let a = blake3_concat(&[b"a"]);
        let b = blake3_concat(&[b"b"]);
        let c = blake3_concat(&[b"c"]);
        let h1 = blake3_concat(&[&a, &b]);
        let h2 = blake3_concat(&[&c, &c]);
        let expected = blake3_concat(&[&h1, &h2]);
        assert_eq!(merkle_root(vec![a, b, c]), expected);
    }

    #[test]
    fn order_matters() {
        let a = blake3_concat(&[b"a"]);
        let b = blake3_concat(&[b"b"]);
        assert_ne!(merkle_root(vec![a, b]), merkle_root(vec![b, a]));
    }
}
