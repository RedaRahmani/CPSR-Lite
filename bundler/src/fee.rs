#[derive(Clone, Debug)]
pub struct FeePlan {
    pub cu_limit: u64,               // post-safety cap
    pub cu_price_microlamports: u64, // priority fee per CU (μLam/CU)
}

pub trait FeeOracle: Send + Sync {
    fn suggest(&self) -> FeePlan;            // initial guess (sender will tighten after simulate)
    fn clamp(&self, plan: FeePlan) -> FeePlan; // enforce floors/ceilings
}

/// Very safe defaults; swap with a congestion-aware oracle later.
pub struct BasicFeeOracle {
    pub max_cu_limit: u64,           // e.g., 1_400_000 (cluster dependent)
    pub min_cu_price: u64,           // μLam/CU floor
    pub max_cu_price: u64,           // μLam/CU ceiling
}
impl Default for BasicFeeOracle {
    fn default() -> Self {
        Self { max_cu_limit: 1_400_000, min_cu_price: 0, max_cu_price: 5_000 }
    }
}
impl FeeOracle for BasicFeeOracle {
    fn suggest(&self) -> FeePlan {
        // start conservative; sender will tighten cu_limit from simulation
        FeePlan { cu_limit: self.max_cu_limit, cu_price_microlamports: self.min_cu_price }
    }
    fn clamp(&self, mut p: FeePlan) -> FeePlan {
        if p.cu_limit > self.max_cu_limit { p.cu_limit = self.max_cu_limit; }
        if p.cu_price_microlamports < self.min_cu_price { p.cu_price_microlamports = self.min_cu_price; }
        if p.cu_price_microlamports > self.max_cu_price { p.cu_price_microlamports = self.max_cu_price; }
        p
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggest_uses_min_price_and_max_limit() {
        let o = BasicFeeOracle { max_cu_limit: 1_400_000, min_cu_price: 5, max_cu_price: 10_000 };
        let p = o.suggest();
        assert_eq!(p.cu_limit, 1_400_000);
        assert_eq!(p.cu_price_microlamports, 5);
    }

    #[test]
    fn clamp_enforces_bounds() {
        let o = BasicFeeOracle::default();
        let p = o.clamp(FeePlan { cu_limit: o.max_cu_limit + 1_000, cu_price_microlamports: o.max_cu_price + 1 });
        assert_eq!(p.cu_limit, o.max_cu_limit);
        assert_eq!(p.cu_price_microlamports, o.max_cu_price);

        let p2 = o.clamp(FeePlan { cu_limit: 0, cu_price_microlamports: 0 });
        assert_eq!(p2.cu_price_microlamports, o.min_cu_price);
    }
}
