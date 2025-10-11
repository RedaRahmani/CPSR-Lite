//! Minimal emitter stub so downstream code has a home for bundle/metadata
//! materialization later. Not wired yet; safe to keep in tree.

#[derive(Clone, Debug, Default)]
pub struct EmittedChunkMeta {
    pub est_cu: u32,
    pub est_msg_bytes: u32,
    pub alt_ro_count: u32,
    pub alt_wr_count: u32,
    pub last_valid_block_height: u64,
}

pub struct Emitter;

impl Emitter {
    pub fn new() -> Self { Self }

    /// Placeholder for future use: in Phase-2 we can materialize a Bundle that
    /// carries the above metadata. Keeping this a no-op to avoid pulling extra
    /// deps into Phase-1.
    pub fn emit_chunk(&self, _meta: &EmittedChunkMeta) {}
}
