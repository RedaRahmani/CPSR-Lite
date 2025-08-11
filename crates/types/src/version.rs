use serde::{Serialize, Deserialize};
use solana_program::pubkey::Pubkey;
use crate::hash::{Hash32, blake3_concat};

/// Minimal state fingerprint used for OCC checks.
/// Keep small: verified on-chain for touched accounts only.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AccountVersion {
    pub key: Pubkey,
    pub lamports: u64,
    pub owner: Pubkey,
    /// 64-bit truncated data hash to keep it compact; compute off-chain via blake3.
    pub data_hash64: u64,
    /// Slot at which this fingerprint was taken (monotonic)
    pub slot: u64,
}

impl AccountVersion {
    pub fn digest(&self) -> Hash32 {
        let mut parts = Vec::with_capacity(4 + 8 + 32 + 8);
        parts.push(self.key.as_ref());
        parts.push(&self.lamports.to_le_bytes());
        parts.push(self.owner.as_ref());
        parts.push(&self.data_hash64.to_le_bytes());
        parts.push(&self.slot.to_le_bytes());
        crate::hash::blake3_concat(&parts.iter().map(|v| v.as_slice()).collect::<Vec<_>>())
    }
}
/*
impl AccountVersion {
    pub fn digest(&self) -> Hash32 {
        let parts: [&[u8]; 5] = [
            self.key.as_ref(),
            &self.lamports.to_le_bytes(),
            self.owner.as_ref(),
            &self.data_hash64.to_le_bytes(),
            &self.slot.to_le_bytes(),
        ];
        blake3_concat(&parts)
    }
}
*/
