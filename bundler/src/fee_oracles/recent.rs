//! A percentile + EMA fee oracle built on `getRecentPrioritizationFees`.
//! See docs in the file header of your previous version; logic unchanged.

use std::sync::{Arc, Mutex};

use solana_client::rpc_client::RpcClient;
use solana_client::rpc_response::RpcPrioritizationFee;
use solana_sdk::pubkey::Pubkey;

use crate::fee::{FeeOracle, FeePlan};
use crate::sender::track_get_recent_prioritization_fees;

// Metrics are centralized in bundler::sender via track_get_recent_prioritization_fees().

#[derive(Clone)]
pub struct RecentFeesOracle {
    pub rpc: Arc<RpcClient>,
    pub probe_accounts: Vec<Pubkey>, // Accounts to probe; can be empty.
    pub max_cu_limit: u64,           // e.g., 1_400_000
    pub min_cu_price: u64,           // μLam/CU floor (e.g., 100)
    pub max_cu_price: u64,           // μLam/CU ceiling
    pub p_primary: f64,              // primary percentile (e.g., 0.75)
    pub p_fallback: f64,             // fallback percentile (e.g., 0.90)
    pub ema_alpha: f64,              // EMA smoothing factor (e.g., 0.20)
    pub hysteresis_bps: u64,         // ignore reprices below this threshold (e.g., 300 = 3%)
    // state
    state: Arc<Mutex<OracleState>>,
}

#[derive(Default, Debug, Clone)]
struct OracleState {
    ema_price: Option<f64>,   // EMA over μLam/CU
    last_output: Option<u64>, // last suggested μLam/CU (post clamp)
}

impl RecentFeesOracle {
    pub fn new(rpc: Arc<RpcClient>, probe_accounts: Vec<Pubkey>) -> Self {
        Self {
            rpc,
            probe_accounts,
            max_cu_limit: 1_400_000,
            min_cu_price: 100, // non-zero default floor (configurable)
            max_cu_price: 5_000,
            p_primary: 0.75,
            p_fallback: 0.90,
            ema_alpha: 0.20,
            hysteresis_bps: 300, // 3%
            state: Arc::new(Mutex::new(OracleState::default())),
        }
    }

    fn fetch_fees(&self) -> Vec<u64> {
        // If probe_accounts is empty, RPC returns recent fees anyway.
        track_get_recent_prioritization_fees();
        match self
            .rpc
            .get_recent_prioritization_fees(&self.probe_accounts)
        {
            Ok(list) if !list.is_empty() => {
                list.into_iter()
                    .map(|RpcPrioritizationFee { prioritization_fee, .. }| prioritization_fee)
                    .collect()
            }
            _ => vec![],
        }
    }

    #[inline]
    fn pct(sorted: &[u64], p: f64) -> Option<u64> {
        if sorted.is_empty() {
            return None;
        }
        let n = sorted.len();
        let idx = ((p.clamp(0.0, 1.0) * (n as f64 - 1.0)).ceil() as usize).min(n - 1);
        Some(sorted[idx])
    }

    fn pick_price_with_ema(&self) -> u64 {
        let mut fees = self.fetch_fees();
        if fees.is_empty() {
            return self.min_cu_price;
        }
        fees.sort_unstable();
        let p75 = Self::pct(&fees, self.p_primary);
        let p90 = Self::pct(&fees, self.p_fallback);

        let raw = match (p75, p90) {
            (Some(x), _) => x,
            (None, Some(y)) => y,
            _ => self.min_cu_price,
        } as f64;

        let mut st = self.state.lock().expect("fee oracle mutex");
        let ema = match st.ema_price {
            None => raw,
            Some(prev) => (self.ema_alpha * raw) + (1.0 - self.ema_alpha) * prev,
        };
        st.ema_price = Some(ema);

        let candidate =
            ema.round()
                .clamp(self.min_cu_price as f64, self.max_cu_price as f64) as u64;
        match st.last_output {
            None => {
                st.last_output = Some(candidate);
                candidate
            }
            Some(last) => {
                let bigger = candidate.max(last);
                let smaller = candidate.min(last);
                let delta_bps = if bigger == 0 {
                    0
                } else {
                    ((bigger - smaller) * 10_000) / bigger
                };
                if delta_bps < self.hysteresis_bps {
                    last
                } else {
                    st.last_output = Some(candidate);
                    candidate
                }
            }
        }
    }
}

impl FeeOracle for RecentFeesOracle {
    fn suggest(&self) -> FeePlan {
        let price = self.pick_price_with_ema();
        FeePlan {
            cu_limit: self.max_cu_limit,
            cu_price_microlamports: price.clamp(self.min_cu_price, self.max_cu_price),
        }
    }

    fn clamp(&self, mut plan: FeePlan) -> FeePlan {
        if plan.cu_limit > self.max_cu_limit {
            plan.cu_limit = self.max_cu_limit;
        }
        if plan.cu_price_microlamports < self.min_cu_price {
            plan.cu_price_microlamports = self.min_cu_price;
        }
        if plan.cu_price_microlamports > self.max_cu_price {
            plan.cu_price_microlamports = self.max_cu_price;
        }
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_client::rpc_client::RpcClient;

    #[test]
    fn percentile_indexing_is_conservative() {
    let _o = RecentFeesOracle::new(Arc::new(RpcClient::new_mock("")), vec![]);
        let mut d = vec![1u64, 2, 3, 10, 20, 100];
        d.sort_unstable();
        assert_eq!(RecentFeesOracle::pct(&d, 0.75).unwrap(), 20);
        assert_eq!(RecentFeesOracle::pct(&d, 0.90).unwrap(), 100);
    }
}
