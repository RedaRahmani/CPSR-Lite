use serde::{Serialize, Deserialize};
use solana_program::{
    pubkey::Pubkey,
    instruction::{Instruction, AccountMeta},
};
use crate::hash::{Hash32, dhash};
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

    /// === CANONICAL INTENT ID (use this in merkle leaves) ===
    ///
    /// Commits to the *instruction semantics only*:
    ///   program_id
    ///   shortvec(num_metas) || metas[(pubkey || flags)]*
    ///     where flags = bit0:is_signer, bit1:is_writable
    ///   shortvec(data_len)  || data
    ///
    /// Rationale:
    ///  - This matches Solana's instruction model (AccountMeta has `pubkey`, `is_signer`, `is_writable`).
    ///  - We exclude actor/priority/expiry because they do not change *what* the instruction does.
    ///    That keeps the commitment focused on execution semantics, which on-chain verification cares about. 
    pub fn id(&self) -> Hash32 {
        // Pre-size: program(32) + metas*(32+1) + data + a few bytes for shortvecs
        let mut buf = Vec::with_capacity(64 + self.ix.accounts.len() * 33 + self.ix.data.len() + 8);

        // 1) program_id
        buf.extend_from_slice(self.ix.program_id.as_ref());

        // 2) metas (shortvec length + entries)
        encode_short_u16(self.ix.accounts.len() as u16, &mut buf);
        for AccountMeta { pubkey, is_signer, is_writable } in &self.ix.accounts {
            buf.extend_from_slice(pubkey.as_ref());
            let mut flags = 0u8;
            if *is_signer   { flags |= 0b0000_0001; }
            if *is_writable { flags |= 0b0000_0010; }
            buf.push(flags);
        }

        // 3) data (shortvec length + bytes)
        encode_short_u16(self.ix.data.len() as u16, &mut buf);
        buf.extend_from_slice(&self.ix.data);

        // Domain-separated BLAKE3
        dhash(b"CPSR:INTENTv1", &[&buf])
    }

    /// Optional: a planner-only fingerprint that *also* commits to actor/priority/expiry.
    /// Use off-chain for caching or logging; don't use for consensus/merkle.
    pub fn planner_fingerprint(&self) -> Hash32 {
        let exp = self.expires_at_slot.unwrap_or(0).to_le_bytes();
        let pri = [self.priority];
        let actor = self.actor.to_bytes();
        dhash(b"CPSR:INTENT_PLAN", &[&actor, &pri, &exp, &self.id()])
    }
}

/// Encode Solana's compact-u16 ("shortvec") length.
/// Solana uses shortvec for vector lengths in instructions/messages. :contentReference[oaicite:1]{index=1}
fn encode_short_u16(mut n: u16, out: &mut Vec<u8>) {
    loop {
        let mut elem = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 { elem |= 0x80; }
        out.push(elem);
        if n == 0 { break; }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;
    use solana_program::pubkey::Pubkey;
    use solana_program::instruction::AccountMeta;

    fn k() -> Pubkey { Pubkey::new_unique() }

    fn ix(program: Pubkey, metas: Vec<AccountMeta>, data: Vec<u8>) -> Instruction {
        Instruction { program_id: program, accounts: metas, data }
    }

    // ----- conflicts() -----

    #[test]
    fn conflicts_logic() {
        let a = k();
        let ro = AccountAccess { pubkey: a, access: AccessKind::ReadOnly };
        let rw = AccountAccess { pubkey: a, access: AccessKind::Writable };
        let b = k();
        let ro_b = AccountAccess { pubkey: b, access: AccessKind::ReadOnly };
        let rw_b = AccountAccess { pubkey: b, access: AccessKind::Writable };

        // same key:
        assert!(!ro.conflicts(&ro));      // RO vs RO => no conflict
        assert!(ro.conflicts(&rw));       // RO vs W  => conflict
        assert!(rw.conflicts(&ro));       // W  vs RO => conflict
        assert!(rw.conflicts(&rw));       // W  vs W  => conflict

        // different keys: no conflict regardless of access
        assert!(!ro.conflicts(&ro_b));
        assert!(!ro.conflicts(&rw_b));
        assert!(!rw.conflicts(&ro_b));
        assert!(!rw.conflicts(&rw_b));
    }

    // ----- accesses_from_ix() -----

    #[test]
    fn accesses_dedup_and_escalate() {
        let program = k();
        let a = k();
        let b = k();

        // Duplicate metas for same key: first RO, then Writable -> should escalate to Writable
        let metas = vec![
            AccountMeta { pubkey: a, is_signer: false, is_writable: false },
            AccountMeta { pubkey: a, is_signer: false, is_writable: true  },
            AccountMeta { pubkey: b, is_signer: false, is_writable: false },
        ];
        let ix = ix(program, metas, vec![]);
        let mut acc = UserIntent::accesses_from_ix(&ix);
        acc.sort_by_key(|x| x.pubkey);

        // Two unique keys a,b; a must be Writable due to escalation
        assert_eq!(acc.len(), 2);
        let a_acc = acc.iter().find(|x| x.pubkey == a).unwrap();
        let b_acc = acc.iter().find(|x| x.pubkey == b).unwrap();
        assert_eq!(a_acc.access, AccessKind::Writable);
        assert_eq!(b_acc.access, AccessKind::ReadOnly);
    }

    // ----- normalized() & validate_accesses_match_ix() -----

    #[test]
    fn normalized_matches_ix() {
        let program = k();
        let a = k(); let b = k(); let c = k();

        // unordered metas, some writable
        let metas = vec![
            AccountMeta { pubkey: c, is_signer: false, is_writable: true  },
            AccountMeta { pubkey: a, is_signer: false, is_writable: false },
            AccountMeta { pubkey: b, is_signer: false, is_writable: false },
        ];
        let ix = ix(program, metas, vec![1,2,3]);
        // start with bogus accesses to ensure normalized() fixes it
        let ui = UserIntent {
            actor: k(),
            ix: ix.clone(),
            accesses: vec![
                AccountAccess { pubkey: b, access: AccessKind::Writable }, // wrong & out of order
                AccountAccess { pubkey: a, access: AccessKind::ReadOnly  },
            ],
            priority: 3,
            expires_at_slot: Some(42),
        };
        let ui_norm = ui.normalized();
        assert!(ui_norm.validate_accesses_match_ix());

        // Also ensure constructor derives the same accesses
        let ui2 = UserIntent::new(k(), ix, 0, None);
        assert!(ui2.validate_accesses_match_ix());
    }

    // ----- id() stability & sensitivity -----

    #[test]
    fn id_equal_when_only_planner_fields_change() {
        let program = k(); let a = k(); let b = k();
        let metas = vec![
            AccountMeta { pubkey: a, is_signer: true,  is_writable: true  },
            AccountMeta { pubkey: b, is_signer: false, is_writable: false },
        ];
        let data = vec![9,9,9];
        let ix = ix(program, metas, data);

        let ui1 = UserIntent::new(k(), ix.clone(), 0, None);
        let mut ui2 = ui1.clone();
        // change actor / priority / expiry only
        ui2.actor = k();
        ui2.priority = 7;
        ui2.expires_at_slot = Some(999);

        assert_eq!(ui1.id(), ui2.id(), "planner-only fields must NOT affect id()");
        assert_ne!(ui1.planner_fingerprint(), ui2.planner_fingerprint(), "planner fingerprint SHOULD change");
    }

    #[test]
    fn id_changes_when_program_changes() {
        let a = k(); let b = k();
        let data = vec![1,2,3];
        let metas = vec![
            AccountMeta { pubkey: a, is_signer: false, is_writable: false },
            AccountMeta { pubkey: b, is_signer: true,  is_writable: true  },
        ];
        let ix1 = ix(k(), metas.clone(), data.clone());
        let ix2 = ix(k(), metas, data);
        let u1 = UserIntent::new(k(), ix1, 0, None);
        let u2 = UserIntent::new(k(), ix2, 0, None);
        assert_ne!(u1.id(), u2.id());
    }

    #[test]
    fn id_changes_when_metas_flags_change() {
        let p = k(); let a = k();
        let m_ro = AccountMeta { pubkey: a, is_signer: false, is_writable: false };
        let m_rw = AccountMeta { pubkey: a, is_signer: false, is_writable: true  };
        let u_ro = UserIntent::new(k(), ix(p, vec![m_ro], vec![0]), 0, None);
        let u_rw = UserIntent::new(k(), ix(p, vec![m_rw], vec![0]), 0, None);
        assert_ne!(u_ro.id(), u_rw.id(), "writable flag must affect id");
    }

    #[test]
    fn id_changes_when_metas_order_changes() {
        let p = k(); let a = k(); let b = k();
        let m1 = AccountMeta { pubkey: a, is_signer: false, is_writable: false };
        let m2 = AccountMeta { pubkey: b, is_signer: true,  is_writable: false };
        let u1 = UserIntent::new(k(), ix(p, vec![m1.clone(), m2.clone()], vec![0]), 0, None);
        let u2 = UserIntent::new(k(), ix(p, vec![m2, m1],                 vec![0]), 0, None);
        assert_ne!(u1.id(), u2.id(), "metas are ordered; order must affect id");
    }

    #[test]
    fn id_changes_when_data_changes() {
        let p = k(); let a = k();
        let meta = AccountMeta { pubkey: a, is_signer: false, is_writable: false };
        let u1 = UserIntent::new(k(), ix(p, vec![meta.clone()], vec![1,2,3]), 0, None);
        let u2 = UserIntent::new(k(), ix(p, vec![meta],        vec![1,2,4]), 0, None);
        assert_ne!(u1.id(), u2.id());
    }

    // ----- shortvec encoding edges -----

    #[test]
    fn encode_short_u16_edges() {
        fn enc(n: u16) -> Vec<u8> { let mut v = Vec::new(); super::encode_short_u16(n, &mut v); v }

        assert_eq!(enc(0),     vec![0x00]);
        assert_eq!(enc(1),     vec![0x01]);
        assert_eq!(enc(127),   vec![0x7f]);                // 1-byte max
        assert_eq!(enc(128),   vec![0x80, 0x01]);          // 2-byte start
        assert_eq!(enc(16383), vec![0xff, 0x7f]);          // 2-byte max (for u16 short)
    }

    #[test]
    fn id_handles_large_meta_counts_shortvec_growth() {
        let p = k();
        // 130 metas -> shortvec length takes 2 bytes (>= 128)
        let mut metas = Vec::with_capacity(130);
        for _ in 0..130 {
            metas.push(AccountMeta { pubkey: k(), is_signer: false, is_writable: false });
        }
        let ui = UserIntent::new(k(), ix(p, metas, vec![0xAB, 0xCD]), 0, None);
        // Just ensure we can compute an id and it's stable across repeats
        let id1 = ui.id();
        let id2 = ui.id();
        assert_eq!(id1, id2);
    }

    // ----- serde round-trip -----

    #[test]
    fn serde_roundtrip_preserves_semantics() {
        let p = k(); let a = k(); let b = k();
        let metas = vec![
            AccountMeta { pubkey: a, is_signer: true,  is_writable: true  },
            AccountMeta { pubkey: b, is_signer: false, is_writable: false },
        ];
        let data = vec![7,7,7];
        let ui = UserIntent::new(k(), ix(p, metas, data), 5, Some(123));

        let json = serde_json::to_string(&ui).expect("serialize");
        let back: UserIntent = serde_json::from_str(&json).expect("deserialize");

        // core semantics intact
        assert_eq!(ui.id(), back.id(), "canonical id must survive serde");
        assert!(back.validate_accesses_match_ix(), "accesses must match ix after serde");
        assert_eq!(ui.actor, back.actor);
        assert_eq!(ui.priority, back.priority);
        assert_eq!(ui.expires_at_slot, back.expires_at_slot);
    }
}
