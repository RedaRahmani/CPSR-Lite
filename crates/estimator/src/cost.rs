use serde::{Deserialize, Serialize};

/// Output of CU safety application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuEstimate {
    pub cu: u32,
    pub cu_with_safety: u32,
}

/// Additive slack + percentage safety (e.g., +10k + 20%).
/// Use this after simulation (units_consumed) and before building the final message.
pub fn apply_safety(cu: u32, slack: u32, pct: u8) -> CuEstimate {
    let pct_add = (cu as u64 * pct as u64) / 100;
    let with = cu.saturating_add(slack).saturating_add(pct_add as u32);
    CuEstimate {
        cu,
        cu_with_safety: with,
    }
}

/// Same as `apply_safety`, then clamp to a hard cap (e.g., cluster limit).
pub fn apply_safety_with_cap(cu: u32, slack: u32, pct: u8, cap: u32) -> CuEstimate {
    let mut est = apply_safety(cu, slack, pct);
    if est.cu_with_safety > cap {
        est.cu_with_safety = cap;
    }
    est
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_safety_adds_slack_and_pct() {
        let x = apply_safety(100_000, 10_000, 20);
        assert_eq!(x.cu, 100_000);
        assert_eq!(x.cu_with_safety, 100_000 + 10_000 + 20_000);
    }

    #[test]
    fn apply_safety_with_cap_clamps() {
        let x = apply_safety_with_cap(800_000, 10_000, 50, 1_000_000);
        assert!(x.cu_with_safety <= 1_000_000);
    }
}
