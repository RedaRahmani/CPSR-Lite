use serde::{Serialize, Deserialize};
use crate::hash::Hash32;
use crate::intent::UserIntent;
use crate::version::AccountVersion;

/// A chunk is the atomic unit we submit (under CU/size limits)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub intents: Vec<UserIntent>,
    /// OCC fingerprints for all accounts this chunk will read/write
    pub occ_versions: Vec<AccountVersion>,
    /// Merkle root over ordered intents for integrity
    pub merkle_root: Hash32,
    /// Optional: session PDA expected, bundle nonce, last valid block height
    pub session_authority: Option<solana_program::pubkey::Pubkey>,
    pub nonce: u64,
    pub last_valid_block_height: u64,
    /// Predicted CU & size to help the coordinator enforce budgets
    pub est_cu: u32,
    pub est_msg_bytes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub chunks: Vec<Chunk>,
    /// Commitment over chunks; used in commit–reveal
    pub bundle_root: Hash32,
    pub created_at_slot: u64,
    pub max_age_slots: u64,
}

impl Bundle {
    pub fn is_expired(&self, current_slot: u64) -> bool {
        current_slot.saturating_sub(self.created_at_slot) > self.max_age_slots
    }
}
