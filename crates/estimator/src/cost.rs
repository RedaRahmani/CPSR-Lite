use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuEstimate {
    pub cu: u32,
    pub cu_with_safety: u32,
}

pub fn apply_safety(cu: u32, slack: u32, pct: u8) -> CuEstimate {
    let pct_add = (cu as u64 * pct as u64) / 100;
    let with = cu.saturating_add(slack).saturating_add(pct_add as u32);
    CuEstimate { cu, cu_with_safety: with }
}
