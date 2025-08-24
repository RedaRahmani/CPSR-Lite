use serde::{Serialize, Deserialize};
use solana_program::{
    pubkey::Pubkey,
    instruction::{Instruction, AccountMeta},
};
use crate::hash::Hash32;
use crate::serde::{serde_pubkey, serde_instruction};

#[repr(u8)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
pub enum AccessKind { ReadOnly = 0, Writable = 1 }

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct AccountAccess {
    #[serde(with = "serde_pubkey")]
    pub pubkey: Pubkey,
    pub access: AccessKind,
}

impl AccountAccess {
    #[inline]
    pub fn conflicts(&self, other: &Self) -> bool {
        // same key, and at least one side writes → conflict (Solana account locking)
        self.pubkey == other.pubkey
            && (self.access == AccessKind::Writable || other.access == AccessKind::Writable)
    }
}

/// Venue-agnostic action with pre-resolved `Instruction` + access classification.
/// `ix` is serialized via our robust adapter (independent of solana-program serde).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserIntent {
    /// Logical owner/payer/authority for session checks
    #[serde(with = "serde_pubkey")]
    pub actor: Pubkey,

    /// The Solana instruction to execute (program + metas + data)
    #[serde(with = "serde_instruction")]
    pub ix: Instruction,

    /// Deterministic access set for planning (deduped, escalated)
    pub accesses: Vec<AccountAccess>,

    /// Planner hints
    pub priority: u8,
    pub expires_at_slot: Option<u64>,
}

impl UserIntent {
    /// Build accesses directly from ix metas, merging dupes and escalating to Writable if needed.
    pub fn accesses_from_ix(ix: &Instruction) -> Vec<AccountAccess> {
        use std::collections::BTreeMap;
        let mut m = BTreeMap::<Pubkey, AccessKind>::new();
        for AccountMeta { pubkey, is_writable, .. } in &ix.accounts {
            let ak = if *is_writable { AccessKind::Writable } else { AccessKind::ReadOnly };
            m.entry(*pubkey)
                .and_modify(|e| { if ak == AccessKind::Writable { *e = ak; } })
                .or_insert(ak);
        }
        m.into_iter()
            .map(|(pubkey, access)| AccountAccess { pubkey, access })
            .collect()
    }

    /// Normalize `self.accesses` to exactly match `ix`.
    pub fn normalized(mut self) -> Self {
        let mut from_ix = Self::accesses_from_ix(&self.ix);
        from_ix.sort_by_key(|x| x.pubkey);
        self.accesses = from_ix;
        self
    }

    /// Quick invariant check – useful for tests and debug assertions.
    pub fn validate_accesses_match_ix(&self) -> bool {
        let mut a = self.accesses.clone(); a.sort_by_key(|x| x.pubkey);
        let mut b = Self::accesses_from_ix(&self.ix); b.sort_by_key(|x| x.pubkey);
        a == b
    }

    /// Convenience constructor that derives `accesses` from `ix`.
    pub fn new(actor: Pubkey, ix: Instruction, priority: u8, expires_at_slot: Option<u64>) -> Self {
        let accesses = Self::accesses_from_ix(&ix);
        let ui = Self { actor, ix, accesses, priority, expires_at_slot };
        debug_assert!(ui.validate_accesses_match_ix());
        ui
    }

    /// Canonical, domain-separated intent ID (stable across platforms).
    /// Commits to: actor, priority/expiry, program_id, metas (key/sign/writable), and raw data.
    pub fn id(&self) -> Hash32 {
        use blake3::Hasher;
        const TAG: &[u8] = b"CPSR:INTENT";

        let mut h = Hasher::new();
        h.update(TAG);

        // planner metadata
        h.update(self.actor.as_ref());
        h.update(&[self.priority]);
        let exp = self.expires_at_slot.unwrap_or(0);
        h.update(&exp.to_le_bytes());

        // instruction (program + metas + data)
        h.update(self.ix.program_id.as_ref());
        for m in &self.ix.accounts {
            h.update(m.pubkey.as_ref());
            h.update(&[u8::from(m.is_signer)]);
            h.update(&[u8::from(m.is_writable)]);
        }
        h.update(&self.ix.data);

        *h.finalize().as_bytes()
    }
}
