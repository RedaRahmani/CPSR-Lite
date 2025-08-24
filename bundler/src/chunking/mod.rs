//! CU/Size/Accounts-aware chunking for CPSR-Lite.
//!
//! Responsibilities
//! - Pack a conflict-free DAG layer (NodeIds) into transaction-sized chunks.
//! - Enforce budgets: message bytes, unique accounts, instruction count, CU.
//! - Stay deterministic: preserve the layer’s order (no re-sorting).
//! - Be future-proof: optional Address Lookup Table (ALT) pre-planning.
//! - Be pluggable: replace heuristics with a real estimator without changing callers.
//!
//! Notes
//! - This module makes conservative size assumptions and includes slack to avoid edge
//!   underestimation. When you wire in your estimator crate, it will get tighter.
//! - ALT policy is optional here and only affects *planning math*. Actual ALT assembly
//!   belongs in serializer/planner; this prepares the counts and enforces limits.

use std::collections::BTreeSet;

use cpsr_types::intent::{AccountAccess, AccessKind};
use cpsr_types::UserIntent;
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;

use crate::dag::NodeId;

#[derive(Debug, Clone)]
pub struct TxBudget {
    /// Conservative max bytes for a MessageV0 (keep buffer under wire limit).
    pub max_bytes: usize,
    /// Extra cushion on top of computed size.
    pub byte_slack: usize,
    /// Max unique account keys included directly in the message key table.
    pub max_unique_accounts: usize,
    /// Max instructions per tx.
    pub max_instructions: usize,
    /// Max CU per tx (budgeted/estimated).
    pub max_cu: u64,
    /// Default CU per intent when no oracle is provided.
    pub default_cu_per_intent: u64,
}

impl Default for TxBudget {
    fn default() -> Self {
        Self {
            max_bytes: 1200,              // conservative under ~1232 packet limit
            byte_slack: 64,               // header/varint cushion
            max_unique_accounts: 48,      // leave headroom under protocol caps
            max_instructions: 30,         // practical limit
            max_cu: 1_200_000,            // under 1.4M
            default_cu_per_intent: 25_000,
        }
    }
}

/// Optional ALT planning. If enabled, we “reserve” a number of message keys and
/// allow additional unique keys to be “offloaded” as ALT candidates.
/// This only affects planning math; actual ALT assembly is done elsewhere.
#[derive(Debug, Clone)]
pub struct AltPolicy {
    pub enabled: bool,
    /// Max keys we plan to keep in the message key table; the rest are ALT candidates.
    pub reserve_message_keys: usize,
    /// Upper bound on how many keys we’re willing to put in ALT (per chunk).
    pub max_alt_keys: usize,
    /// Extra overhead (bytes) we budget per ALT key (conservative).
    pub overhead_per_alt_key_bytes: usize,
}

impl Default for AltPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            reserve_message_keys: 32,     // reserve message table for hot/early keys
            max_alt_keys: 64,             // generous ceiling; tune later
            overhead_per_alt_key_bytes: 2 // conservative tiny header/indices overhead
        }
    }
}

/// Pluggable budget oracle for size/CU.
/// Replace with your estimator without changing callers.
pub trait BudgetOracle {
    /// Encoded bytes for the compiled instruction payload (program index + acct idxs + data).
    fn instr_bytes(&self, ix: &Instruction) -> usize;
    /// CU estimate for a UserIntent.
    fn cu_for_intent(&self, _intent: &UserIntent) -> u64;
}

/// Simple conservative oracle (no external crates).
pub struct BasicOracle;

impl BudgetOracle for BasicOracle {
    #[inline]
    fn instr_bytes(&self, ix: &Instruction) -> usize {
        // program_id_index (u8) + shortvec(accounts) + accounts + shortvec(data) + data
        1 + shortvec_len(ix.accounts.len()) + ix.accounts.len()
            + shortvec_len(ix.data.len()) + ix.data.len()
    }
    #[inline]
    fn cu_for_intent(&self, _intent: &UserIntent) -> u64 {
        // Caller’s TxBudget.default_cu_per_intent is used in the accumulator.
        // We return 0 here and let the accumulator add default if needed.
        0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    #[error("single intent exceeds tx budgets for node={node_id:?} (bytes={bytes}, message_unique={message_unique}, alt_unique={alt_unique}, instrs=1, cu={cu})")]
    IntentTooLarge {
        node_id: NodeId,
        bytes: usize,
        message_unique: usize,
        alt_unique: usize,
        cu: u64,
    },
}

/// Backwards-compatible API: no ALT, BasicOracle.
pub fn chunk_layer(
    layer: &[NodeId],
    intents: &[UserIntent],
    budget: &TxBudget,
) -> Result<Vec<Vec<NodeId>>, ChunkError> {
    chunk_layer_with(layer, intents, budget, &AltPolicy::default(), &BasicOracle)
}

/// Extended API with ALT policy and a pluggable oracle.
pub fn chunk_layer_with<O: BudgetOracle>(
    layer: &[NodeId],
    intents: &[UserIntent],
    budget: &TxBudget,
    alt: &AltPolicy,
    oracle: &O,
) -> Result<Vec<Vec<NodeId>>, ChunkError> {
    // Precompute per-intent footprints once.
    let mut fps = Vec::with_capacity(layer.len());
    for &nid in layer {
        let intent = &intents[nid as usize];
        fps.push(IntentFootprint::from_intent(intent, budget, oracle));
    }

    let mut chunks: Vec<Vec<NodeId>> = Vec::new();
    let mut cur = ChunkAccumulator::new(budget, alt);

    for (i, &nid) in layer.iter().enumerate() {
        let fp = &fps[i];

        // If the single intent cannot fit in an empty tx (respecting ALT policy), error.
        if !fits_in_empty(fp, budget, alt) {
            return Err(ChunkError::IntentTooLarge {
                node_id: nid,
                bytes: fp.self_bytes_with_base(budget, alt),
                message_unique: fp.message_unique_len(alt),
                alt_unique: fp.alt_unique_len(alt),
                cu: fp.cu_estimate(budget),
            });
        }

        // Try to add to the current chunk; if not possible, seal and start a new one.
        if !cur.can_add(fp) {
            if !cur.nodes.is_empty() {
                chunks.push(std::mem::take(&mut cur.nodes));
                cur.reset(budget, alt);
            }
        }
        debug_assert!(cur.can_add(fp)); // must fit in empty
        cur.add(nid, fp);
    }

    if !cur.nodes.is_empty() {
        chunks.push(cur.nodes);
    }

    Ok(chunks)
}

// ===== Internals =====

const BASE_MESSAGE_OVERHEAD: usize = 100;
const PER_INSTRUCTION_OVERHEAD: usize = 2;

#[inline]
fn shortvec_len(n: usize) -> usize {
    if n < 128 { 1 } else if n < 16384 { 2 } else { 3 }
}

#[derive(Debug, Clone)]
struct IntentFootprint {
    /// All keys referenced by this instruction: program + metas (ordered).
    keys: Vec<Pubkey>,
    /// Encoded instruction payload bytes.
    instr_bytes: usize,
    /// CU estimate for this intent.
    intent_cu: u64,
}

impl IntentFootprint {
    fn from_intent<O: BudgetOracle>(intent: &UserIntent, budget: &TxBudget, oracle: &O) -> Self {
        let ix = &intent.ix;

        // Keep deterministic order: program first, then metas as given.
        let mut keys = Vec::with_capacity(ix.accounts.len() + 1);
        keys.push(ix.program_id);
        for m in &ix.accounts { keys.push(m.pubkey); }

        let mut cu = oracle.cu_for_intent(intent);
        if cu == 0 { cu = budget.default_cu_per_intent; }

        Self {
            keys,
            instr_bytes: oracle.instr_bytes(ix),
            intent_cu: cu,
        }
    }

    #[inline]
    fn message_unique_len(&self, alt: &AltPolicy) -> usize {
        if !alt.enabled { return self.keys.len(); }
        self.keys.len().min(alt.reserve_message_keys)
    }

    #[inline]
    fn alt_unique_len(&self, alt: &AltPolicy) -> usize {
        if !alt.enabled { return 0; }
        self.keys.len().saturating_sub(alt.reserve_message_keys)
    }

    #[inline]
    fn cu_estimate(&self, _budget: &TxBudget) -> u64 {
        self.intent_cu
    }

    fn self_bytes_with_base(&self, budget: &TxBudget, alt: &AltPolicy) -> usize {
        BASE_MESSAGE_OVERHEAD
            + (self.message_unique_len(alt) * 32)
            + PER_INSTRUCTION_OVERHEAD
            + self.instr_bytes
            + budget.byte_slack
            + self.alt_unique_len(alt) * alt.overhead_per_alt_key_bytes
    }
}

fn fits_in_empty(fp: &IntentFootprint, budget: &TxBudget, alt: &AltPolicy) -> bool {
    let bytes = fp.self_bytes_with_base(budget, alt);
    let msg_uniq = fp.message_unique_len(alt);
    let alt_uniq = fp.alt_unique_len(alt);
    bytes <= budget.max_bytes
        && msg_uniq <= budget.max_unique_accounts
        && alt_uniq <= alt.max_alt_keys
        && 1 <= budget.max_instructions
        && fp.cu_estimate(budget) <= budget.max_cu
}

struct ChunkAccumulator<'a> {
    nodes: Vec<NodeId>,
    /// Keys included in the message key table (deterministic).
    message_keys: BTreeSet<Pubkey>,
    /// Keys planned to be sourced from ALT (if enabled); counted but not serialized here.
    alt_keys: BTreeSet<Pubkey>,
    bytes: usize,
    cu: u64,
    instrs: usize,
    budget: &'a TxBudget,
    alt: &'a AltPolicy,
}

impl<'a> ChunkAccumulator<'a> {
    fn new(budget: &'a TxBudget, alt: &'a AltPolicy) -> Self {
        Self {
            nodes: Vec::new(),
            message_keys: BTreeSet::new(),
            alt_keys: BTreeSet::new(),
            bytes: BASE_MESSAGE_OVERHEAD + budget.byte_slack,
            cu: 0,
            instrs: 0,
            budget,
            alt,
        }
    }

    fn reset(&mut self, budget: &'a TxBudget, alt: &'a AltPolicy) {
        self.nodes.clear();
        self.message_keys.clear();
        self.alt_keys.clear();
        self.bytes = BASE_MESSAGE_OVERHEAD + budget.byte_slack;
        self.cu = 0;
        self.instrs = 0;
        self.budget = budget;
        self.alt = alt;
    }

    fn deltas_for(&self, fp: &IntentFootprint) -> (usize, usize, usize, u64) {
        // Determine how many of fp.keys become message keys vs ALT keys.
        let mut new_message_keys = 0usize;
        let mut new_alt_keys = 0usize;

        for k in &fp.keys {
            let in_msg = self.message_keys.contains(k);
            let in_alt = self.alt_keys.contains(k);

            if in_msg || in_alt {
                continue;
            }

            if self.alt.enabled {
                // Current effective message capacity already used:
                let current_msg_cap = self.message_keys.len().min(self.alt.reserve_message_keys);
                if current_msg_cap < self.alt.reserve_message_keys {
                    new_message_keys += 1;
                } else {
                    new_alt_keys += 1;
                }
            } else {
                new_message_keys += 1;
            }
        }

        // Bytes contributed by new unique message keys + this instruction payload.
        let add_bytes = (new_message_keys * 32)
            + PER_INSTRUCTION_OVERHEAD
            + fp.instr_bytes
            + (new_alt_keys * self.alt.overhead_per_alt_key_bytes);

        let add_instrs = 1usize;
        let add_cu = fp.cu_estimate(self.budget);

        (add_bytes, new_message_keys + new_alt_keys, add_instrs, add_cu)
    }

    fn can_add(&self, fp: &IntentFootprint) -> bool {
        let (add_bytes, add_unique_total, add_instrs, add_cu) = self.deltas_for(fp);

        let (prospective_msg_keys, prospective_alt_keys) = if self.alt.enabled {
            // Split the “add_unique_total” into message vs alt derived from actual keys:
            // Recompute split deterministically to check caps.
            let mut new_msg = 0usize;
            let mut new_alt = 0usize;
            for k in &fp.keys {
                if self.message_keys.contains(k) || self.alt_keys.contains(k) { continue; }
                let current_msg_cap = self.message_keys.len().min(self.alt.reserve_message_keys);
                if current_msg_cap < self.alt.reserve_message_keys { new_msg += 1; }
                else { new_alt += 1; }
            }
            (self.message_keys.len() + new_msg, self.alt_keys.len() + new_alt)
        } else {
            (self.message_keys.len() + add_unique_total, self.alt_keys.len())
        };

        (self.bytes + add_bytes) <= self.budget.max_bytes
            && prospective_msg_keys <= self.budget.max_unique_accounts
            && prospective_alt_keys <= self.alt.max_alt_keys
            && (self.instrs + add_instrs) <= self.budget.max_instructions
            && (self.cu + add_cu) <= self.budget.max_cu
    }

    fn add(&mut self, nid: NodeId, fp: &IntentFootprint) {
        // Update key sets deterministically.
        for k in &fp.keys {
            if self.message_keys.contains(k) || self.alt_keys.contains(k) { continue; }
            if self.alt.enabled {
                let current_msg_cap = self.message_keys.len().min(self.alt.reserve_message_keys);
                if current_msg_cap < self.alt.reserve_message_keys {
                    self.message_keys.insert(*k);
                } else {
                    self.alt_keys.insert(*k);
                }
            } else {
                self.message_keys.insert(*k);
            }
        }
        let (add_bytes, _add_unique_total, add_instrs, add_cu) = self.deltas_for(fp);
        self.bytes = self.bytes.saturating_add(add_bytes);
        self.instrs = self.instrs.saturating_add(add_instrs);
        self.cu = self.cu.saturating_add(add_cu);
        self.nodes.push(nid);
    }
}

// ===== Tests =====

#[cfg(test)]
mod tests {
    use super::*;
    use solana_program::instruction::AccountMeta;

    fn mk_intent(keys: &[Pubkey], data_len: usize) -> UserIntent {
        let program_id = keys[0];
        let accounts = keys[1..]
            .iter()
            .map(|k| AccountMeta::new(*k, false))
            .collect::<Vec<_>>();
        let ix = Instruction { program_id, accounts, data: vec![0u8; data_len] };
        UserIntent {
            actor: Pubkey::new_unique(),
            ix,
            accesses: keys[1..].iter().map(|k| AccountAccess { pubkey: *k, access: AccessKind::ReadOnly }).collect(),
            priority: 0,
            expires_at_slot: None,
        }
    }

    #[test]
    fn packs_together_when_safe() {
        let p = Pubkey::new_unique();
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();

        let intents = vec![mk_intent(&[p, a], 8), mk_intent(&[p, b], 8)];
        let layer: Vec<NodeId> = vec![0, 1];
        let budget = TxBudget { max_bytes: 600, ..Default::default() };

        let chunks = chunk_layer(&layer, &intents, &budget).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], vec![0, 1]);
    }

    #[test]
    fn splits_when_unique_accounts_exceed() {
        let p = Pubkey::new_unique();
        // Each intent introduces many unique keys.
        let intents = (0..3).map(|_| {
            let mut keys = vec![p];
            for _ in 0..30 { keys.push(Pubkey::new_unique()); }
            mk_intent(&keys, 8)
        }).collect::<Vec<_>>();

        let layer: Vec<NodeId> = vec![0, 1, 2];
        let budget = TxBudget { max_bytes: 20_000, max_unique_accounts: 48, ..Default::default() };

        let chunks = chunk_layer(&layer, &intents, &budget).unwrap();
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn errors_if_single_intent_too_large() {
        let p = Pubkey::new_unique();
        let intent = mk_intent(&[p, Pubkey::new_unique()], 10_000);
        let layer = vec![0];
        let budget = TxBudget { max_bytes: 500, ..Default::default() };

        let err = chunk_layer(&layer, &[intent], &budget).unwrap_err();
        matches!(err, ChunkError::IntentTooLarge { .. });
    }

    #[test]
    fn alt_policy_allows_offloading_keys() {
        let p = Pubkey::new_unique();
        let mut keys = vec![p];
        // 40 account metas → 41 keys including program.
        for _ in 0..40 { keys.push(Pubkey::new_unique()); }
        let intent = mk_intent(&keys, 8);

        let layer = vec![0];
        let budget = TxBudget { max_bytes: 10_000, max_unique_accounts: 32, ..Default::default() };
        let alt = AltPolicy { enabled: true, reserve_message_keys: 32, max_alt_keys: 64, overhead_per_alt_key_bytes: 2 };

        // Should succeed because we reserve 32 in message and offload the rest to ALT.
        let chunks = chunk_layer_with(&layer, &[intent], &budget, &alt, &BasicOracle).unwrap();
        assert_eq!(chunks.len(), 1);
    }
}
