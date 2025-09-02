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

// ---------- Budgets ----------

#[derive(Debug, Clone)]
pub struct TxBudget {
    /// Conservative max bytes for the Message (caller should pre-subtract signatures).
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
            max_bytes: 1200,              // caller should refine using estimator::SafetyMargins helper
            byte_slack: 64,               // cushion
            max_unique_accounts: 48,      // practical ceiling
            max_instructions: 30,         // way below shortvec bump
            max_cu: 1_200_000,
            default_cu_per_intent: 25_000,
        }
    }
}

/// ALT policy & cost model — updated:
#[derive(Debug, Clone)]
pub struct AltPolicy {
    pub enabled: bool,
    /// Keep up to this many keys in the static message key table; offload the rest.
    pub reserve_message_keys: usize,
    /// Upper bound on how many keys we’re willing to put in ALT (per chunk).
    pub max_alt_keys: usize,
    /// Estimated number of distinct lookup tables used (affects fixed overhead).
    pub table_count_estimate: usize,
    /// Per-table fixed overhead: table pubkey(32) + 2*shortvec(len) ~ 34 bytes.
    pub per_table_fixed_bytes: usize,
    /// Per ALT index cost (u8 index) — 1 byte.
    pub per_alt_index_byte: usize,
}

impl Default for AltPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            reserve_message_keys: 32,
            max_alt_keys: 64,
            table_count_estimate: 1,
            per_table_fixed_bytes: 34,
            per_alt_index_byte: 1,
        }
    }
}

// ---------- Oracle ----------

pub trait BudgetOracle {
    fn instr_bytes(&self, ix: &Instruction) -> usize;
    fn cu_for_intent(&self, _intent: &UserIntent) -> u64;
}

pub struct BasicOracle;
impl BudgetOracle for BasicOracle {
    #[inline]
    fn instr_bytes(&self, ix: &Instruction) -> usize {
        // compiled instruction: program idx(1) + sv(accs) + accs + sv(data) + data
        #[inline] fn sv(n: usize) -> usize { if n < 128 { 1 } else if n < 16384 { 2 } else { 3 } }
        let accs = ix.accounts.len();
        let data = ix.data.len();
        1 + sv(accs) + accs + sv(data) + data
    }
    #[inline]
    fn cu_for_intent(&self, _intent: &UserIntent) -> u64 { 0 }
}

// ---------- Errors ----------

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

// ---------- API (unchanged signatures) ----------

pub fn chunk_layer(
    layer: &[NodeId],
    intents: &[UserIntent],
    budget: &TxBudget,
) -> Result<Vec<Vec<NodeId>>, ChunkError> {
    chunk_layer_with(layer, intents, budget, &AltPolicy::default(), &BasicOracle)
}

pub fn chunk_layer_with<O: BudgetOracle>(
    layer: &[NodeId],
    intents: &[UserIntent],
    budget: &TxBudget,
    alt: &AltPolicy,
    oracle: &O,
) -> Result<Vec<Vec<NodeId>>, ChunkError> {
    let mut fps = Vec::with_capacity(layer.len());
    for &nid in layer {
        let intent = &intents[nid as usize];
        fps.push(IntentFootprint::from_intent(intent, budget, oracle));
    }

    let mut chunks: Vec<Vec<NodeId>> = Vec::new();
    let mut cur = ChunkAccumulator::new(budget, alt);

    for (i, &nid) in layer.iter().enumerate() {
        let fp = &fps[i];

        if !fits_in_empty(fp, budget, alt) {
            return Err(ChunkError::IntentTooLarge {
                node_id: nid,
                bytes: fp.self_bytes_with_base(budget, alt),
                message_unique: fp.message_unique_len(alt),
                alt_unique: fp.alt_unique_len(alt),
                cu: fp.cu_estimate(budget),
            });
        }

        if !cur.can_add(fp) {
            if !cur.nodes.is_empty() {
                chunks.push(std::mem::take(&mut cur.nodes));
                cur.reset(budget, alt);
            }
        }
        debug_assert!(cur.can_add(fp));
        cur.add(nid, fp);
    }

    if !cur.nodes.is_empty() {
        chunks.push(cur.nodes);
    }

    Ok(chunks)
}

// ---------- Internals ----------

// Message v0 base, excluding keys length (we’ll approximate len=1 byte)
// header(3) + blockhash(32) + sv(keys)(1) + sv(lookups)(1) + sv(instrs)(1)
const BASE_MESSAGE_OVERHEAD: usize = 3 + 32 + 1 + 1 + 1;

#[inline]
fn shortvec_len(n: usize) -> usize {
    if n < 128 { 1 } else if n < 16384 { 2 } else { 3 }
}

#[derive(Debug, Clone)]
struct IntentFootprint {
    keys: Vec<Pubkey>,     // program + metas (ordered)
    instr_bytes: usize,    // compiled instruction bytes
    intent_cu: u64,        // CU estimate
}

impl IntentFootprint {
    fn from_intent<O: BudgetOracle>(intent: &UserIntent, budget: &TxBudget, oracle: &O) -> Self {
        let ix = &intent.ix;

        let mut keys = Vec::with_capacity(ix.accounts.len() + 1);
        keys.push(ix.program_id);
        for m in &ix.accounts { keys.push(m.pubkey); }

        let mut cu = oracle.cu_for_intent(intent);
        if cu == 0 { cu = budget.default_cu_per_intent; }

        Self { keys, instr_bytes: oracle.instr_bytes(ix), intent_cu: cu }
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
    fn cu_estimate(&self, _budget: &TxBudget) -> u64 { self.intent_cu }

    fn self_bytes_with_base(&self, budget: &TxBudget, alt: &AltPolicy) -> usize {
        BASE_MESSAGE_OVERHEAD
            + (self.message_unique_len(alt) * 32)     // static keys
            + self.instr_bytes                         // compiled ix bytes
            + budget.byte_slack
            + (self.alt_unique_len(alt) * alt.per_alt_index_byte) // ALT indices (u8 each)
            // NOTE: per-table fixed overhead is added once in the accumulator when ALT is first used
    }
}

fn fits_in_empty(fp: &IntentFootprint, budget: &TxBudget, alt: &AltPolicy) -> bool {
    let bytes = fp.self_bytes_with_base(budget, alt)
        + if alt.enabled && fp.alt_unique_len(alt) > 0 {
            // lookup tables vector len (1) + estimated per-table fixed bytes
            1 + alt.table_count_estimate * alt.per_table_fixed_bytes
        } else { 0 };

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
    message_keys: BTreeSet<Pubkey>,
    alt_keys: BTreeSet<Pubkey>,
    bytes: usize,
    cu: u64,
    instrs: usize,
    alt_used: bool,
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
            alt_used: false,
            budget, alt,
        }
    }

    fn reset(&mut self, budget: &'a TxBudget, alt: &'a AltPolicy) {
        self.nodes.clear();
        self.message_keys.clear();
        self.alt_keys.clear();
        self.bytes = BASE_MESSAGE_OVERHEAD + budget.byte_slack;
        self.cu = 0;
        self.instrs = 0;
        self.alt_used = false;
        self.budget = budget;
        self.alt = alt;
    }

    fn deltas_for(&self, fp: &IntentFootprint) -> (usize, usize, usize, u64, bool) {
        let mut new_message_keys = 0usize;
        let mut new_alt_keys = 0usize;

        for k in &fp.keys {
            if self.message_keys.contains(k) || self.alt_keys.contains(k) { continue; }

            if self.alt.enabled {
                let used_in_msg = self.message_keys.len().min(self.alt.reserve_message_keys);
                if used_in_msg < self.alt.reserve_message_keys { new_message_keys += 1; }
                else { new_alt_keys += 1; }
            } else {
                new_message_keys += 1;
            }
        }

        // Bytes contributed by new static keys + this compiled instruction + ALT index bytes.
        let mut add_bytes = (new_message_keys * 32) + fp.instr_bytes + (new_alt_keys * self.alt.per_alt_index_byte);

        // If ALT becomes used for the first time, add vector len(1) + estimated per-table fixed bytes.
        let alt_will_be_used = self.alt.enabled && (self.alt_keys.is_empty() && new_alt_keys > 0);
        if alt_will_be_used {
            add_bytes += 1 + self.alt.table_count_estimate * self.alt.per_table_fixed_bytes;
        }

        let add_instrs = 1usize;
        let add_cu = fp.cu_estimate(self.budget);

        (add_bytes, new_message_keys + new_alt_keys, add_instrs, add_cu, alt_will_be_used)
    }

    fn can_add(&self, fp: &IntentFootprint) -> bool {
        let (add_bytes, add_unique_total, add_instrs, add_cu, _alt_first) = self.deltas_for(fp);

        let (prospective_msg_keys, prospective_alt_keys) = if self.alt.enabled {
            let mut new_msg = 0usize;
            let mut new_alt = 0usize;
            for k in &fp.keys {
                if self.message_keys.contains(k) || self.alt_keys.contains(k) { continue; }
                let used = self.message_keys.len().min(self.alt.reserve_message_keys);
                if used < self.alt.reserve_message_keys { new_msg += 1; } else { new_alt += 1; }
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
        for k in &fp.keys {
            if self.message_keys.contains(k) || self.alt_keys.contains(k) { continue; }
            let used = self.message_keys.len().min(self.alt.reserve_message_keys);
            if self.alt.enabled && used >= self.alt.reserve_message_keys {
                self.alt_keys.insert(*k);
            } else {
                self.message_keys.insert(*k);
            }
        }
        let (add_bytes, _add_unique_total, add_instrs, add_cu, alt_first) = self.deltas_for(fp);
        self.bytes = self.bytes.saturating_add(add_bytes);
        self.instrs = self.instrs.saturating_add(add_instrs);
        self.cu = self.cu.saturating_add(add_cu);
        if alt_first { self.alt_used = true; }
        self.nodes.push(nid);
    }
}




#[cfg(test)]
mod more_chunk_tests {
    use super::*;
    use solana_program::instruction::AccountMeta;

    fn mk_intent(keys: &[Pubkey], data_len: usize) -> UserIntent {
        let program_id = keys[0];
        let accounts = keys[1..].iter().map(|k| AccountMeta::new(*k, false)).collect::<Vec<_>>();
        let ix = Instruction { program_id, accounts, data: vec![0u8; data_len] };
        UserIntent::new(Pubkey::new_unique(), ix, 0, None)
    }

    #[test]
    fn splits_on_bytes_limit() {
        let p = Pubkey::new_unique();
        let a = Pubkey::new_unique();

        // Two intents large enough to force separate chunks given max_bytes
        let intents = vec![mk_intent(&[p, a], 700), mk_intent(&[p, a], 700)];
        let layer: Vec<NodeId> = vec![0, 1];

        let mut budget = TxBudget::default();
        budget.max_bytes = 900; // intentionally low
        let chunks = chunk_layer(&layer, &intents, &budget).unwrap();
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn splits_on_cu_limit() {
        let p = Pubkey::new_unique();
        let a = Pubkey::new_unique();
        let intents = vec![mk_intent(&[p, a], 8), mk_intent(&[p, a], 8), mk_intent(&[p, a], 8)];
        let layer: Vec<NodeId> = vec![0, 1, 2];

        let mut budget = TxBudget::default();
        budget.max_bytes = 10_000;
        budget.max_cu = budget.default_cu_per_intent; // allow exactly one intent per chunk
        let chunks = chunk_layer(&layer, &intents, &budget).unwrap();
        assert_eq!(chunks.len(), 3);
    }
}
