use solana_program::{instruction::Instruction, message::v0::MessageAddressTableLookup};
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizeEstimate {
    pub instr_bytes: u32,
    pub account_metas: u32,
    pub approx_v0_bytes: u32,
}

pub fn estimate_instruction_size(ix: &Instruction) -> SizeEstimate {
    // Very conservative: program_id + num_accounts*(pubkey + flags) + data len
    let instr_bytes = (32 + 1 /*vec len*/ + ix.accounts.len() as u32 * (32 + 2) + 1 + ix.data.len() as u32) as u32;
    // Account metas count
    let account_metas = ix.accounts.len() as u32 + 1; // + program id
    // Rough message size (single-instruction baseline)
    let approx_v0_bytes = instr_bytes + 64; // headers etc. (tune empirically)
    SizeEstimate { instr_bytes, account_metas, approx_v0_bytes }
}

/// Combine multiple size estimates and add slack for ALTs and headers.
pub fn fold_message_size(estimates: &[SizeEstimate], bytes_slack: u32) -> u32 {
    let sum = estimates.iter().map(|e| e.approx_v0_bytes).sum::<u32>();
    sum + bytes_slack
}
