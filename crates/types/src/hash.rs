use blake3::Hasher;

// Small helper code to create unique fingerprints of data (like giving each package a unique tracking code).
// If we change the package later, the fingerprint will change — this lets us detect tampering or mistakes.
// Where it’s used: In Merkle trees, in checking account versions, and in making bundle IDs.

pub type Hash32 = [u8; 32];

pub fn blake3_hash(bytes: &[u8]) -> Hash32 {
    *blake3::hash(bytes).as_bytes()
}

pub fn blake3_concat(parts: &[&[u8]]) -> Hash32 {
    let mut h = Hasher::new();
    for p in parts { h.update(p); }
    *h.finalize().as_bytes()
}
