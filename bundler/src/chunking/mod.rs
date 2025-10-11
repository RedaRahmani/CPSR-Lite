//! CU/Size/Accounts-aware chunking for CPSR-Lite.
//
// (unchanged module docs)

use std::collections::BTreeSet;

use cpsr_types::UserIntent;
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;

use crate::dag::NodeId;

#[derive(Debug, Clone)]
pub struct PlannedChunk {
    pub node_ids: Vec<NodeId>,
    pub est_cu: u64,
    pub est_message_bytes: usize,
    pub alt_readonly: usize,
    pub alt_writable: usize,
}

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

/// v1 heuristic CU oracle:
/// - baseline per instruction
/// - + per-account cost (heavier for writable)
/// - + data-size slope
///
/// This is intentionally simple and deterministic. Replace with program-aware tables later.
#[derive(Debug, Clone, Default)]
pub struct ProgramAwareOracle;

impl BudgetOracle for ProgramAwareOracle {
    #[inline]
    fn instr_bytes(&self, ix: &Instruction) -> usize {
        // compiled instruction: program idx(1) + sv(accs) + accs + sv(data) + data
        #[inline] fn sv(n: usize) -> usize { if n < 128 { 1 } else if n < 16384 { 2 } else { 3 } }
        let accs = ix.accounts.len();
        let data = ix.data.len();
        1 + sv(accs) + accs + sv(data) + data
    }

    #[inline]
    fn cu_for_intent(&self, intent: &UserIntent) -> u64 {
        // Baselines chosen conservatively to avoid underestimation spikes.
        // You’ll later swap to a table keyed by (program, ix discriminant) + params.
        let accs = &intent.ix.accounts;
        let data_len = intent.ix.data.len();

        let base: u64 = 12_000; // baseline per ix
        let mut per_acc: u64 = 0;
        for m in accs {
            per_acc += if m.is_writable { 2_500 } else { 1_000 };
        }
        let data_term: u64 = (data_len as u64).saturating_mul(30); // ~30 CU per byte (heuristic)

        base + per_acc + data_term
    }
}

/// Legacy trivial oracle (kept for tests/back-compat).
pub struct BasicOracle;
impl BudgetOracle for BasicOracle {
    #[inline]
    fn instr_bytes(&self, ix: &Instruction) -> usize {
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
) -> Result<Vec<PlannedChunk>, ChunkError> {
    // Use the smarter oracle by default.
    let oracle = ProgramAwareOracle::default();
    chunk_layer_with(layer, intents, budget, &AltPolicy::default(), &oracle)
}

pub fn chunk_layer_with<O: BudgetOracle>(
    layer: &[NodeId],
    intents: &[UserIntent],
    budget: &TxBudget,
    alt: &AltPolicy,
    oracle: &O,
) -> Result<Vec<PlannedChunk>, ChunkError> {
    let mut fps = Vec::with_capacity(layer.len());
    for &nid in layer {
        let intent = &intents[nid as usize];
        fps.push(IntentFootprint::from_intent(intent, budget, oracle));
    }

    let mut chunks: Vec<PlannedChunk> = Vec::new();
    let mut cur = ChunkAccumulator::new(budget, alt);

    for (i, &nid) in layer.iter().enumerate() {
        let fp = &fps[i];

        if !fits_in_empty(fp, budget, alt) {
            return Err(ChunkError::IntentTooLarge {
                node_id: nid,
                bytes: size_if_only_this(fp, budget, alt),
                message_unique: fp.message_unique_len(alt),
                alt_unique: fp.alt_unique_len(alt),
                cu: fp.cu_estimate(budget),
            });
        }

        if !cur.can_add(fp) {
            if !cur.nodes.is_empty() {
                let chunk = cur.take_planned_chunk();
                chunks.push(chunk);
                cur.reset(budget, alt);
            }
        }
        debug_assert!(cur.can_add(fp));
        cur.add(nid, fp);
    }

    if !cur.nodes.is_empty() {
        let chunk = cur.take_planned_chunk();
        chunks.push(chunk);
    }

    Ok(chunks)
}

// ---------- Internals ----------

// Message v0 base excluding *all* shortvec lengths (we compute them exactly).
// header(3) + blockhash(32)
const BASE_MESSAGE_OVERHEAD: usize = 3 + 32;

#[inline]
fn shortvec_len(n: usize) -> usize {
    if n < 128 { 1 } else if n < 16384 { 2 } else { 3 }
}

#[inline]
fn delta_sv(old: usize, new: usize) -> usize {
    shortvec_len(new).saturating_sub(shortvec_len(old))
}

#[derive(Debug, Clone)]
struct KeyMeta {
    pubkey: Pubkey,
    writable: bool,
}

struct IntentFootprint {
    keys: Vec<KeyMeta>,    // program + metas (ordered)
    instr_bytes: usize,    // compiled instruction bytes
    intent_cu: u64,        // CU estimate
}

impl IntentFootprint {
    fn from_intent<O: BudgetOracle>(intent: &UserIntent, budget: &TxBudget, oracle: &O) -> Self {
        let ix = &intent.ix;

        let mut keys = Vec::with_capacity(ix.accounts.len() + 1);
        keys.push(KeyMeta { pubkey: ix.program_id, writable: false });
        for m in &ix.accounts {
            keys.push(KeyMeta { pubkey: m.pubkey, writable: m.is_writable });
        }

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

    /// Bytes contributed *by this intent only*:
    /// - static keys (32B each) kept in message table
    /// - compiled ix bytes
    /// - ALT index bytes (u8 per offloaded key)
    ///
    /// NOTE: does NOT include:
    /// - BASE_MESSAGE_OVERHEAD
    /// - shortvec lengths (keys/lookups/instrs) → those depend on global counts and are handled in accumulator.
    fn self_bytes(&self, alt: &AltPolicy) -> usize {
        (self.message_unique_len(alt) * 32)
            + self.instr_bytes
            + (self.alt_unique_len(alt) * alt.per_alt_index_byte)
    }
}

fn size_if_only_this(fp: &IntentFootprint, budget: &TxBudget, alt: &AltPolicy) -> usize {
    // Exact size if a transaction contained only this intent.
    // BASE + slack + sv(keys=msg_uniq) + sv(lookups) + sv(instrs=1) + self-bytes + (ALT table fixed if used)
    let msg_uniq = fp.message_unique_len(alt);
    let alt_uniq = fp.alt_unique_len(alt);
    let sv_keys = shortvec_len(msg_uniq);
    let sv_instrs = shortvec_len(1);
    // lookups vector always present in v0; 0 → 1 byte, else grows as needed
    let table_count = if alt.enabled && alt_uniq > 0 { alt.table_count_estimate } else { 0 };
    let sv_lookups = shortvec_len(table_count);

    BASE_MESSAGE_OVERHEAD
        + budget.byte_slack
        + sv_keys
        + sv_lookups
        + sv_instrs
        + fp.self_bytes(alt)
        + if table_count > 0 { table_count * alt.per_table_fixed_bytes } else { 0 }
}

fn fits_in_empty(fp: &IntentFootprint, budget: &TxBudget, alt: &AltPolicy) -> bool {
    let bytes = size_if_only_this(fp, budget, alt);

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
    alt_ro_keys: BTreeSet<Pubkey>,
    alt_wr_keys: BTreeSet<Pubkey>,
    bytes: usize,
    cu: u64,
    instrs: usize,
    alt_used: bool,
    budget: &'a TxBudget,
    alt: &'a AltPolicy,
}

impl<'a> ChunkAccumulator<'a> {
    fn new(budget: &'a TxBudget, alt: &'a AltPolicy) -> Self {
        // Start with BASE + slack + sv(keys=0) + sv(lookups=0) + sv(instrs=0)
        let bytes = BASE_MESSAGE_OVERHEAD
            + budget.byte_slack
            + shortvec_len(0)  // keys
            + shortvec_len(0)  // lookups
            + shortvec_len(0); // instrs

        Self {
            nodes: Vec::new(),
            message_keys: BTreeSet::new(),
            alt_keys: BTreeSet::new(),
            alt_ro_keys: BTreeSet::new(),
            alt_wr_keys: BTreeSet::new(),
            bytes,
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
        self.alt_ro_keys.clear();
        self.alt_wr_keys.clear();
        self.bytes = BASE_MESSAGE_OVERHEAD
            + budget.byte_slack
            + shortvec_len(0)  // keys
            + shortvec_len(0)  // lookups
            + shortvec_len(0); // instrs
        self.cu = 0;
        self.instrs = 0;
        self.alt_used = false;
        self.budget = budget;
        self.alt = alt;
    }

    fn deltas_for(&self, fp: &IntentFootprint) -> (usize, usize, usize, u64, bool) {
        let mut new_message_keys = 0usize;
        let mut new_alt_keys = 0usize;

        for meta in &fp.keys {
            let key = meta.pubkey;
            if self.message_keys.contains(&key) || self.alt_keys.contains(&key) { continue; }

            if self.alt.enabled {
                let used_in_msg = self.message_keys.len().min(self.alt.reserve_message_keys);
                if used_in_msg < self.alt.reserve_message_keys { new_message_keys += 1; }
                else { new_alt_keys += 1; }
            } else {
                new_message_keys += 1;
            }
        }

        // Bytes from this intent itself.
        let mut add_bytes = fp.self_bytes(self.alt);

        // Shortvec deltas:
        // keys shortvec tracks *static message keys only*
        let old_msg_keys = self.message_keys.len();
        let new_msg_keys_total = old_msg_keys + new_message_keys;
        add_bytes += delta_sv(old_msg_keys, new_msg_keys_total);

        // instrs shortvec
        add_bytes += delta_sv(self.instrs, self.instrs + 1);

        // lookups shortvec + per-table fixed, when ALT toggles from 0 → >0
        let alt_will_be_used = self.alt.enabled && (self.alt_keys.is_empty() && new_alt_keys > 0);
        if alt_will_be_used {
            // shortvec(len) change from 0 → table_count_estimate
            add_bytes += delta_sv(0, self.alt.table_count_estimate);
            // add per-table fixed bytes
            add_bytes += self.alt.table_count_estimate * self.alt.per_table_fixed_bytes;
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
            for meta in &fp.keys {
                let key = meta.pubkey;
                if self.message_keys.contains(&key) || self.alt_keys.contains(&key) { continue; }
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
        for meta in &fp.keys {
            let key = meta.pubkey;
            if self.message_keys.contains(&key) || self.alt_keys.contains(&key) { continue; }
            let used = self.message_keys.len().min(self.alt.reserve_message_keys);
            if self.alt.enabled && used >= self.alt.reserve_message_keys {
                self.alt_keys.insert(key);
                if meta.writable { self.alt_wr_keys.insert(key); } else { self.alt_ro_keys.insert(key); }
            } else {
                self.message_keys.insert(key);
            }
        }
        let (add_bytes, _add_unique_total, add_instrs, add_cu, alt_first) = self.deltas_for(fp);
        self.bytes = self.bytes.saturating_add(add_bytes);
        self.instrs = self.instrs.saturating_add(add_instrs);
        self.cu = self.cu.saturating_add(add_cu);
        if alt_first { self.alt_used = true; }
        self.nodes.push(nid);
    }

    fn take_planned_chunk(&mut self) -> PlannedChunk {
        PlannedChunk {
            node_ids: std::mem::take(&mut self.nodes),
            est_cu: self.cu,
            est_message_bytes: self.bytes,
            alt_readonly: self.alt_ro_keys.len(),
            alt_writable: self.alt_wr_keys.len(),
        }
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
        assert_eq!(chunks[0].node_ids, vec![0]);
        assert_eq!(chunks[1].node_ids, vec![1]);
        assert!(chunks[0].est_message_bytes > 0);
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
        for (idx, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.node_ids, vec![idx as NodeId]);
        }
    }
}
