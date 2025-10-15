use blake3::Hasher;

/// 32-byte hash
pub type Hash32 = [u8; 32];

/// All-zero hash (handy default/root)
pub const ZERO32: Hash32 = [0u8; 32];

/// Hash a single blob
pub fn blake3_hash(bytes: &[u8]) -> Hash32 {
    *blake3::hash(bytes).as_bytes()
}

/// Hash multiple parts by concatenation (streamed to avoid big allocations)
pub fn blake3_concat(parts: &[&[u8]]) -> Hash32 {
    let mut h = Hasher::new();
    for p in parts {
        h.update(p);
    }
    *h.finalize().as_bytes()
}

/// Domain-separated hash: `tag || payload...`
/// (easy to audit; avoids cross-context collisions)
pub fn dhash(tag: &'static [u8], payloads: &[&[u8]]) -> Hash32 {
    let mut h = Hasher::new();
    h.update(tag);
    for p in payloads {
        h.update(p);
    }
    *h.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero32_is_all_zeroes() {
        assert_eq!(ZERO32.len(), 32);
        assert!(ZERO32.iter().all(|&b| b == 0));
    }

    #[test]
    fn dhash_domain_separates() {
        let payload = b"payload";
        let a = dhash(b"TAG-A", &[payload]);
        let b = dhash(b"TAG-B", &[payload]);
        assert_ne!(a, b);
        assert_eq!(a, dhash(b"TAG-A", &[payload]));
    }

    #[test]
    fn blake3_concat_equals_whole_blob() {
        let a = b"hello";
        let b = b"world";
        let x = blake3_concat(&[a, b]);

        let mut joined = Vec::new();
        joined.extend_from_slice(a);
        joined.extend_from_slice(b);
        let y = blake3_hash(&joined);
        assert_eq!(x, y);
    }
}
