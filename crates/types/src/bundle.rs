use crate::hash::{dhash, Hash32, ZERO32};
use crate::intent::UserIntent;
use crate::version::AccountVersion;
use serde::{Deserialize, Serialize};

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
    pub alt_ro_count: u32,
    pub alt_wr_count: u32,
    #[serde(default)]
    pub plan_fingerprint: Option<Hash32>,
}

impl Chunk {
    /// Compute Merkle over OCC digests (sorted for determinism).
    pub fn occ_root(&self) -> Hash32 {
        let mut occ = self.occ_versions.clone();
        occ.sort(); // uses Ord (key, then slot)
        let leaves: Vec<Hash32> = occ.iter().map(|v| v.digest()).collect();
        crate::merkle::merkle_root(leaves)
    }

    /// Compute Merkle over ordered intent ids (does NOT mutate self).
    pub fn intent_root(&self) -> Hash32 {
        let leaves: Vec<Hash32> = self.intents.iter().map(|i| i.id()).collect();
        crate::merkle::merkle_root(leaves)
    }

    /// Deterministic chunk identifier: commits to intent root + OCC set root.
    pub fn id(&self) -> Hash32 {
        dhash(b"CPSR:CHUNK", &[&self.merkle_root, &self.occ_root()])
    }

    /// OPTIONAL: a stronger binding that also commits to session + nonce (if present).
    pub fn id_bound(&self) -> Hash32 {
        let sess = self
            .session_authority
            .map(|k| k.to_bytes())
            .unwrap_or([0u8; 32]);
        let nonce_bytes = self.nonce.to_le_bytes();
        let plan = self.plan_fingerprint.unwrap_or(ZERO32);
        dhash(
            b"CPSR:CHUNKBOUND",
            &[
                &self.merkle_root,
                &self.occ_root(),
                &sess,
                &nonce_bytes,
                &plan,
            ],
        )
    }

    /// Normalize internal ordering for deterministic commits (safe to call anytime).
    pub fn normalize(&mut self) {
        self.occ_versions.sort(); // key, then slot
                                  // `intents` remain in insertion order by design.
    }

    /// Recompute `merkle_root` from `intents` – call after edits.
    pub fn recompute_merkle_root(&mut self) {
        self.merkle_root = self.intent_root();
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

    /// OPTIONAL convenience: a human-friendly handle identical to bundle_root.
    pub fn id(&self) -> Hash32 {
        self.bundle_root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_program::pubkey::Pubkey;

    #[test]
    fn chunk_id_stable_under_occ_permutation() {
        // same versions, different internal order -> same id()
        let c1 = Chunk {
            intents: vec![],
            occ_versions: vec![
                AccountVersion {
                    key: Pubkey::new_unique(),
                    lamports: 1,
                    owner: Pubkey::new_unique(),
                    data_hash64: 10,
                    slot: 5,
                },
                AccountVersion {
                    key: Pubkey::new_unique(),
                    lamports: 2,
                    owner: Pubkey::new_unique(),
                    data_hash64: 20,
                    slot: 7,
                },
            ],
            merkle_root: [1u8; 32],
            session_authority: None,
            nonce: 0,
            last_valid_block_height: 0,
            est_cu: 0,
            est_msg_bytes: 0,
            alt_ro_count: 0,
            alt_wr_count: 0,
            plan_fingerprint: None,
        };
        let mut c2 = c1.clone();
        c2.occ_versions.swap(0, 1);

        // normalize or rely on occ_root() internal sort: both should match
        let id1 = c1.id();
        let id2 = c2.id();
        assert_eq!(id1, id2);
    }
}

#[cfg(test)]
mod more_tests {
    use super::*;
    use crate::intent::UserIntent;
    use solana_program::instruction::{AccountMeta, Instruction};
    use solana_program::pubkey::Pubkey;

    fn mk_intent(program: Pubkey, keys: &[Pubkey], data: Vec<u8>) -> UserIntent {
        let metas: Vec<AccountMeta> = keys
            .iter()
            .map(|k| AccountMeta::new_readonly(*k, false))
            .collect();
        let ix = Instruction {
            program_id: program,
            accounts: metas,
            data,
        };
        UserIntent::new(Pubkey::new_unique(), ix, 0, None)
    }

    #[test]
    fn intent_and_occ_roots_and_ids() {
        let p = Pubkey::new_unique();
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let mut c = Chunk {
            intents: vec![mk_intent(p, &[a], vec![1]), mk_intent(p, &[b], vec![2])],
            occ_versions: vec![],
            merkle_root: [0u8; 32],
            session_authority: None,
            nonce: 0,
            last_valid_block_height: 0,
            est_cu: 0,
            est_msg_bytes: 0,
            alt_ro_count: 0,
            alt_wr_count: 0,
            plan_fingerprint: None,
        };
        c.recompute_merkle_root();
        let id1 = c.id();
        assert_ne!(id1, [0u8; 32]);

        c.session_authority = Some(Pubkey::new_unique());
        let bound = c.id_bound();
        assert_ne!(
            bound, id1,
            "binding to session/nonce should change id_bound()"
        );
    }

    #[test]
    fn bundle_root_changes_with_chunk_order() {
        use crate::hash::ZERO32;
        use solana_program::instruction::AccountMeta;
        use solana_program::pubkey::Pubkey; // if you export it; otherwise just use [0u8; 32]

        // Helper to build a one-intent chunk whose instruction differs by `data` byte.
        fn mk_chunk_with_data_byte(b: u8) -> Chunk {
            let program = Pubkey::new_unique();
            let acc = Pubkey::new_unique();
            let ix = solana_program::instruction::Instruction {
                program_id: program,
                accounts: vec![AccountMeta::new(acc, false)],
                data: vec![b],
            };
            let intent = crate::intent::UserIntent::new(Pubkey::new_unique(), ix, 0, None);
            let mut c = Chunk {
                intents: vec![intent],
                occ_versions: vec![], // empty → same OCC root (ZERO32), which is fine
                merkle_root: ZERO32,
                session_authority: None,
                nonce: 0,
                last_valid_block_height: 0,
                est_cu: 0,
                est_msg_bytes: 0,
                alt_ro_count: 0,
                alt_wr_count: 0,
                plan_fingerprint: None,
            };
            c.recompute_merkle_root();
            c
        }

        let c1 = mk_chunk_with_data_byte(0x01);
        let c2 = mk_chunk_with_data_byte(0x02);

        // Bundle [c1, c2]
        let mut b12 = Bundle {
            chunks: vec![c1.clone(), c2.clone()],
            bundle_root: [0u8; 32],
            created_at_slot: 0,
            max_age_slots: 0,
        };
        b12.recompute_root();

        // Bundle [c2, c1]
        let mut b21 = Bundle {
            chunks: vec![c2, c1],
            bundle_root: [0u8; 32],
            created_at_slot: 0,
            max_age_slots: 0,
        };
        b21.recompute_root();

        // Order must matter now because Chunk::id() differs between c1 and c2.
        assert_ne!(b12.bundle_root, b21.bundle_root);
    }
}
