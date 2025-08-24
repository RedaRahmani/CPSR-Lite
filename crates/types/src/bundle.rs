use serde::{Serialize, Deserialize};
use crate::hash::{Hash32, dhash};
use crate::intent::UserIntent;
use crate::version::AccountVersion;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// Intents in insertion order (order matters for the intent Merkle)
    pub intents: Vec<UserIntent>,

    /// OCC fingerprints for all accounts this chunk touches
    pub occ_versions: Vec<AccountVersion>,

    /// Merkle root over ordered intents (use `UserIntent::id()` as leaves)
    pub merkle_root: Hash32,

    /// Optional: session PDA expected
    #[serde(with = "crate::serde::serde_pubkey_opt")]
    pub session_authority: Option<solana_program::pubkey::Pubkey>,

    /// Replay/ordering helpers
    pub nonce: u64,
    pub last_valid_block_height: u64,

    /// Hints only (for pricing/planning) – not consensus
    pub est_cu: u32,
    pub est_msg_bytes: u32,
}

impl Chunk {
    /// Deterministic chunk identifier: commits to intent root + OCC set root.
    pub fn id(&self) -> Hash32 {
        // OCC digests → sort for determinism → Merkle root
        let mut occ = self.occ_versions.clone();
        occ.sort();
        let occ_hashes: Vec<Hash32> = occ.iter().map(|v| v.digest()).collect();
        let occ_root = crate::merkle::merkle_root(occ_hashes);

        // Combine intent root + OCC root under a domain tag
        dhash(b"CPSR:CHUNK", &[&self.merkle_root, &occ_root])
    }

    /// Recompute `merkle_root` from `intents` (using `Intent::id()`) – call after edits.
    pub fn recompute_merkle_root(&mut self) {
        let leaves: Vec<Hash32> = self.intents.iter().map(|i| i.id()).collect();
        self.merkle_root = crate::merkle::merkle_root(leaves);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub chunks: Vec<Chunk>,
    /// Commitment over chunk IDs (domain-separated)
    pub bundle_root: Hash32,
    pub created_at_slot: u64,
    pub max_age_slots: u64,
}

impl Bundle {
    /// Compute Merkle root over chunk IDs.
    pub fn compute_root(chunks: &[Chunk]) -> Hash32 {
        let ids: Vec<Hash32> = chunks.iter().map(|c| c.id()).collect();
        crate::merkle::merkle_root(ids)
    }

    pub fn recompute_root(&mut self) {
        self.bundle_root = Self::compute_root(&self.chunks);
    }

    pub fn is_expired(&self, current_slot: u64) -> bool {
        current_slot.saturating_sub(self.created_at_slot) > self.max_age_slots
    }
}
