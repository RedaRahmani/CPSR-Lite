use serde::{Serialize, Deserialize};
use solana_program::{pubkey::Pubkey, instruction::Instruction};

/// Access intent: do we plan to read or write an account?
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AccessKind { ReadOnly, Writable }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountAccess {
    pub pubkey: Pubkey,
    pub access: AccessKind,
}

/// Two Writable ops on the same account = conflict.
/// ReadOnly with Writable on the same account = conflict (writer vs reader).
/// ReadOnly with ReadOnly on the same account = no conflict.


/// A user-intent is a venue-agnostic description of an action we’ll execute.
/// It includes the pre-resolved Instruction and the access it needs for planning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserIntent {
    /// Who is the logical owner of this action (payer/authority). Used for session checks.
    pub actor: Pubkey,
    /// The Solana instruction to run (already encoded).
    pub ix: Instruction,
    /// Planner-usable account access classification for OCC & DAG
    pub accesses: Vec<AccountAccess>,
    /// Optional: grouping key, priority, expiry slot, etc.
    pub priority: u8,
    pub expires_at_slot: Option<u64>,
}
