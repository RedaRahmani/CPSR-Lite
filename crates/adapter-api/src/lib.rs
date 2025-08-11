use solana_program::{instruction::{AccountMeta, Instruction}, pubkey::Pubkey};
use serde::{Serialize, Deserialize};
use cpsr_types::{UserIntent, AccountVersion, Hash32};

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("invalid parameters: {0}")]
    InvalidParams(String),
    #[error("estimation failed: {0}")]
    Estimation(String),
    #[error("fingerprint failed: {0}")]
    Fingerprint(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterPlan {
    /// Final instruction to be executed via CPI by the coordinator.
    pub ix: Instruction,
    /// Read/write classification for planning.
    pub accesses: Vec<cpsr_types::intent::AccountAccess>,
    /// For OCC: state fingerprints of critical accounts (venue-dependent)
    pub occ_versions: Vec<AccountVersion>,
    /// CU prediction for this op alone (bundler will aggregate)
    pub est_cu: u32,
    /// Message size estimate if executed standalone (used to approximate chunk size)
    pub est_msg_bytes: u32,
    /// Optional: human-friendly label for metrics
    pub label: String,
}

pub trait Adapter {
    /// Build an instruction from high-level params (e.g., swap ‘A->B, amount’).
    /// Return a fully-formed plan with OCC fingerprints and estimates.
    fn plan_intent(&self, params: serde_json::Value) -> Result<AdapterPlan, AdapterError>;

    /// For intents provided externally (already constructed instruction),
    /// classify access and estimate CU/size. Useful for generic passthrough.
    fn analyze_instruction(&self, ix: &Instruction) -> Result<AdapterPlan, AdapterError>;

    /// Optional: preflight sim on your own RPC or heuristic CU estimate.
    fn simulate_cu(&self, ix: &Instruction) -> Result<u32, AdapterError>;
}
