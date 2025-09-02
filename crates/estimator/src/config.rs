use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyMargins {
    /// Additive CU buffer
    pub cu_slack: u32,
    /// Multiply CU by (1 + pct/100.0)
    pub cu_safety_pct: u8,
    /// Max CU per chunk/tx (cap after safety)
    pub max_chunk_cu: u32,

    /// Extra slack applied to message bytes (safety cushion)
    pub bytes_slack: u32,
    /// Upper bound for message bytes before clamping to packet MTU (keep conservative)
    pub max_msg_bytes: u32,
}

impl SafetyMargins {
    /// Solana packet MTU for (signatures + message) is 1232 bytes. The message budget
    /// is 1232 - (shortvec(num_sigs) + 64*num_sigs) - bytes_slack. :contentReference[oaicite:0]{index=0}
    pub fn max_message_bytes_with_signers(&self, num_signers: usize) -> usize {
        const PACKET_DATA_SIZE: usize = 1232;
        fn shortvec_len(n: usize) -> usize { if n < 128 { 1 } else if n < 16384 { 2 } else { 3 } }
        let sig_bytes = 64usize * num_signers + shortvec_len(num_signers);
        let hard = PACKET_DATA_SIZE.saturating_sub(sig_bytes);
        hard
            .min(self.max_msg_bytes as usize)
            .saturating_sub(self.bytes_slack as usize)
    }
}

impl Default for SafetyMargins {
    fn default() -> Self {
        Self {
            cu_slack: 10_000,
            cu_safety_pct: 20,
            max_chunk_cu: 1_000_000,
            bytes_slack: 64,
            max_msg_bytes: 1200,
        }
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_message_bytes_reserves_signers_and_slack() {
        let s = SafetyMargins::default();
        let m1 = s.max_message_bytes_with_signers(1);
        // upper bounded by (MTU - sig bytes) and by max_msg_bytes - slack
        assert!(m1 <= (s.max_msg_bytes - s.bytes_slack) as usize);

        let m2 = s.max_message_bytes_with_signers(2);
        assert!(m2 < m1, "more signers -> fewer message bytes");

        // sanity for large shortvec jump
        let m128 = s.max_message_bytes_with_signers(128);
        assert!(m128 <= m2);
    }

    #[test]
    fn constant_aligned_with_packet_limit() {
        let s = SafetyMargins::default();
        let n = 1usize;
        let shortvec = if n < 128 { 1 } else if n < 16384 { 2 } else { 3 };
        let expected_upper = 1232usize.saturating_sub(64 * n + shortvec);
        assert!(s.max_message_bytes_with_signers(n) <= expected_upper);
    }
}
