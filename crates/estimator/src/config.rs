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
    /// Max v0 message bytes (before ALT resolution) — *upper bound*, we still clamp to packet MTU.
    pub max_msg_bytes: u32,
}

impl SafetyMargins {
    /// Solana packet MTU — total (sigs + message) must fit. Docs: 1232 bytes. :contentReference[oaicite:6]{index=6}
    pub const PACKET_MTU: usize = 1232;

    /// Conservative message budget after reserving space for `expected_signers`.
    /// Note: signatures = 64 bytes each; count prefix encoded as shortvec.
    pub fn max_message_bytes_with_signers(&self, expected_signers: usize) -> usize {
        fn shortvec_len(n: usize) -> usize { if n < 128 { 1 } else if n < 16384 { 2 } else { 3 } }
        let sig_bytes = shortvec_len(expected_signers) + expected_signers * 64;
        let hard = Self::PACKET_MTU.saturating_sub(sig_bytes);
        hard.min(self.max_msg_bytes as usize)
    }
}

impl Default for SafetyMargins {
    fn default() -> Self {
        Self {
            cu_slack: 10_000,
            cu_safety_pct: 20,
            max_chunk_cu: 1_000_000, // tune per cluster settings
            bytes_slack: 256,
            max_msg_bytes: 1200, // conservative envelope; clamp by PACKET_MTU at use-site
        }
    }
}
