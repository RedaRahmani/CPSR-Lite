use blake3::Hasher;

pub type Hash32 = [u8; 32];

pub fn blake3_hash(bytes: &[u8]) -> Hash32 {
    *blake3::hash(bytes).as_bytes()
}

pub fn blake3_concat(parts: &[&[u8]]) -> Hash32 {
    let mut h = Hasher::new();
    for p in parts { h.update(p); }
    *h.finalize().as_bytes()
}
