use serde::{Serialize, Deserialize};
use solana_program::instruction::Instruction;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizeEstimate {
    /// Compiled-instruction bytes (program index + account indices + data)
    pub instr_bytes: u32,
    /// Account metas count (indices length); informative only
    pub account_metas: u32,
    /// For compatibility; equal to instr_bytes (legacy field)
    pub approx_v0_bytes: u32,
}

#[inline]
fn shortvec_len(n: usize) -> u32 { if n < 128 { 1 } else if n < 16384 { 2 } else { 3 } }

/// Compiled instruction size: 1 (program idx) + sv(accs) + accs + sv(data) + data_len
#[inline]
pub fn compiled_ix_bytes(ix: &Instruction) -> u32 {
    let accs = ix.accounts.len();
    let data_len = ix.data.len();
    (1 + shortvec_len(accs) + accs as u32 + shortvec_len(data_len) + data_len as u32) as u32
}

pub fn estimate_instruction_size(ix: &Instruction) -> SizeEstimate {
    let bytes = compiled_ix_bytes(ix);
    SizeEstimate {
        instr_bytes: bytes,
        account_metas: (ix.accounts.len() + 1) as u32, // + program
        approx_v0_bytes: bytes,
    }
}

/// Shape for a v0 message when you know how many static keys and lookup-table indices you’ll use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V0MessageShape {
    /// unique static keys in the message key table
    pub message_keys: usize,
    /// one entry per lookup table: (readonly_indices, writable_indices)
    pub lookup_tables: Vec<(usize, usize)>,
    /// number of instructions
    pub instr_count: usize,
}

/// Sum of compiled instruction bytes.
#[inline]
pub fn sum_compiled_ix_bytes(ixs: &[Instruction]) -> u32 {
    ixs.iter().map(compiled_ix_bytes).sum()
}

/// Estimate full v0 message bytes (not including signatures).
pub fn estimate_message_v0_bytes(ixs: &[Instruction], shape: &V0MessageShape) -> u32 {
    // Fixed parts:
    // header(3) + blockhash(32) + sv(keys) + 32*keys + sv(lookups) + per-table + sv(instr_count) + compiled_ixs
    let header = 3u32;
    let blockhash = 32u32;
    let keys_len = shortvec_len(shape.message_keys);
    let keys_bytes = (shape.message_keys as u32) * 32;

    let lookup_len = shortvec_len(shape.lookup_tables.len());
    let mut lookup_bytes = 0u32;
    for (ro, wr) in &shape.lookup_tables {
        lookup_bytes += 32; // table pubkey
        lookup_bytes += shortvec_len(*ro) + (*ro as u32); // indices as u8 each
        lookup_bytes += shortvec_len(*wr) + (*wr as u32);
    }

    let instr_len = shortvec_len(shape.instr_count);
    let instr_bytes = sum_compiled_ix_bytes(ixs);

    header + blockhash + keys_len + keys_bytes + lookup_len + lookup_bytes + instr_len + instr_bytes
}

/// Combine with slack (e.g., for unknown extra headers or minor underestimates).
pub fn fold_message_size(estimates: &[SizeEstimate], bytes_slack: u32) -> u32 {
    let sum = estimates.iter().map(|e| e.instr_bytes).sum::<u32>();
    sum + bytes_slack
}
