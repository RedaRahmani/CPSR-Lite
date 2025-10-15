use crate::hash::{blake3_concat, Hash32};
use serde::{Deserialize, Serialize};
use solana_program::pubkey::Pubkey;

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
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    fn k() -> Pubkey {
        Pubkey::new_unique()
    }

    #[test]
    fn digest_changes_when_any_field_changes() {
        let base = AccountVersion {
            key: k(),
            lamports: 1,
            owner: k(),
            data_hash64: 42,
            slot: 10,
        };
        let d0 = base.digest();

        let mut v = base;
        v.lamports = 2;
        assert_ne!(d0, v.digest());
        v = base;
        v.owner = k();
        assert_ne!(d0, v.digest());
        v = base;
        v.data_hash64 = 43;
        assert_ne!(d0, v.digest());
        v = base;
        v.slot = 11;
        assert_ne!(d0, v.digest());
        v = base;
        v.key = k();
        assert_ne!(d0, v.digest());
    }

    #[test]
    fn ord_sorts_by_key_then_slot() {
        let key = k();
        let owner = k();
        let a = AccountVersion {
            key,
            lamports: 0,
            owner,
            data_hash64: 0,
            slot: 5,
        };
        let b = AccountVersion {
            key,
            lamports: 0,
            owner,
            data_hash64: 0,
            slot: 7,
        };
        let c = AccountVersion {
            key: k(),
            lamports: 0,
            owner,
            data_hash64: 0,
            slot: 1,
        };
        let mut v = vec![b, c, a];
        v.sort();
        let same_key: Vec<_> = v.iter().filter(|x| x.key == key).cloned().collect();
        assert_eq!(same_key[0].slot, 5);
        assert_eq!(same_key[1].slot, 7);
    }

    #[test]
    fn serde_json_uses_base58_pubkeys_and_roundtrips() {
        let v = AccountVersion {
            key: k(),
            lamports: 3,
            owner: k(),
            data_hash64: 9,
            slot: 77,
        };
        let j = serde_json::to_string(&v).unwrap();
        assert!(j.contains(&format!("\"{}\"", v.key.to_string())));
        assert!(j.contains(&format!("\"{}\"", v.owner.to_string())));
        let back: AccountVersion = serde_json::from_str(&j).unwrap();
        assert_eq!(back, v);
    }
}
