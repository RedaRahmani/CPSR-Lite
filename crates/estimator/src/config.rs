use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyMargins {
    /// Additive CU buffer
    pub cu_slack: u32,
    /// Multiply CU by (1 + pct/100.0)
    pub cu_safety_pct: u8,
    /// Max CU per chunk
    pub max_chunk_cu: u32,

    /// Message size slack in bytes
    pub bytes_slack: u32,
    /// Max v0 message bytes (before ALT resolution)
    pub max_msg_bytes: u32,
}

impl Default for SafetyMargins {
    fn default() -> Self {
        Self {
            cu_slack: 10_000,
            cu_safety_pct: 20,
            max_chunk_cu: 1_000_000, // tune per cluster settings
            bytes_slack: 256,
            max_msg_bytes: 1200, // conservative pre-ALT; adjust with measurements
        }
    }
}
