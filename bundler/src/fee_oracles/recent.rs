//! A fee oracle that probes `getRecentPrioritizationFees` and chooses a price.
//!
//! Strategy:
//! - Query recent priority fees for a set of "probe" accounts (ideally keys you
//!   plan to lock in your chunk). If the RPC call fails or returns nothing,
//!   fallback to `min_cu_price`.
//! - Always clamp price/limit to configured bounds.
//!
//! References:
//! - `simulateTransaction` exposes `unitsConsumed` we already use to size CU. :contentReference[oaicite:0]{index=0}
//! - `isBlockhashValid` to check recency before send. :contentReference[oaicite:1]{index=1}
//! - Packet size budgeting (1232 bytes total; we subtract signatures). :contentReference[oaicite:2]{index=2}
//! - `getRecentPrioritizationFees` API (Rust client docs & usage). :contentReference[oaicite:3]{index=3}
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_response::RpcPrioritizationFee;
use solana_sdk::pubkey::Pubkey;

use crate::fee::{FeeOracle, FeePlan};

#[derive(Clone)]
pub struct RecentFeesOracle {
    pub rpc: RpcClient,
    pub probe_accounts: Vec<Pubkey>,  // accounts you expect to lock
    pub max_cu_limit: u64,            // e.g., 1_400_000
    pub min_cu_price: u64,            // μLam/CU floor
    pub max_cu_price: u64,            // μLam/CU ceiling
}

impl RecentFeesOracle {
    pub fn new(rpc: RpcClient, probe_accounts: Vec<Pubkey>) -> Self {
        Self {
            rpc,
            probe_accounts,
            max_cu_limit: 1_400_000,
            min_cu_price: 0,
            max_cu_price: 5_000,
        }
    }

    fn fetch_suggested_price(&self) -> u64 {
        // If the node is <1.16 this may not be supported; fall back on error.
        match self.rpc.get_recent_prioritization_fees(&self.probe_accounts) {
            Ok(list) if !list.is_empty() => {
                // Choose the maximum recent fee across probed accounts as a conservative floor.
                list.iter()
                    .map(|RpcPrioritizationFee { prioritization_fee, .. }| *prioritization_fee as u64)
                    .max()
                    .unwrap_or(self.min_cu_price)
            }
            _ => self.min_cu_price,
        }
    }
}

impl FeeOracle for RecentFeesOracle {
    fn suggest(&self) -> FeePlan {
        let mut price = self.fetch_suggested_price();
        if price < self.min_cu_price { price = self.min_cu_price; }
        if price > self.max_cu_price { price = self.max_cu_price; }
        FeePlan {
            cu_limit: self.max_cu_limit,
            cu_price_microlamports: price,
        }
    }

    fn clamp(&self, mut plan: FeePlan) -> FeePlan {
        if plan.cu_limit > self.max_cu_limit { plan.cu_limit = self.max_cu_limit; }
        if plan.cu_price_microlamports < self.min_cu_price { plan.cu_price_microlamports = self.min_cu_price; }
        if plan.cu_price_microlamports > self.max_cu_price { plan.cu_price_microlamports = self.max_cu_price; }
        plan
    }
}
