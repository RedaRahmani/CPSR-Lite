use serde::{Serialize, Deserialize};
use solana_program::pubkey::Pubkey;
use crate::hash::{Hash32, blake3_concat};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
pub struct AccountVersion {
    #[serde(with = "crate::serde::serde_pubkey")]
    pub key: Pubkey,
    pub lamports: u64,
    #[serde(with = "crate::serde::serde_pubkey")]
    pub owner: Pubkey,
    /// 64-bit truncated data hash (computed off-chain)
    pub data_hash64: u64,
    /// Slot at which this snapshot was taken
    pub slot: u64,
}

impl Ord for AccountVersion {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.key.cmp(&other.key).then(self.slot.cmp(&other.slot))
    }
}
impl PartialOrd for AccountVersion {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) }
}

impl AccountVersion {
    /// Domain-separated digest for OCC tree leaves.
    pub fn digest(&self) -> Hash32 {
        const TAG: &[u8] = b"CPSR:ACCV1";
        let parts: [&[u8]; 6] = [
            TAG,
            self.key.as_ref(),
            &self.lamports.to_le_bytes(),
            self.owner.as_ref(),
            &self.data_hash64.to_le_bytes(),
            &self.slot.to_le_bytes(),
        ];
        blake3_concat(&parts)
    }
}
