//! RPC-backed AccountFetcher for OCC capture.
//
// Uses `getMultipleAccounts` and stamps each result with the response slot.
// Commitment is configurable; defaults to `processed` if the caller passes that.
//
// Slot-drift acceptance:
// - We compare the latest slot (at the same commitment) with the response slot
//   returned by getMultipleAccounts.
// - If `latest_slot - response_slot <= max_slot_drift`, the capture is "accepted".
// - We track acceptance ratio over a sliding window and warn if it drops < 98%.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use solana_client::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};

use crate::occ::{AccountFetcher, FetchedAccount, OccError};

#[derive(Clone)]
pub struct RpcAccountFetcher {
    rpc: Arc<RpcClient>,
    commitment: CommitmentConfig,
    max_slot_drift: u64,
    stats: Arc<Mutex<DriftStats>>,
}

// Simple sliding stats (last N captures).
#[derive(Default)]
struct DriftStats {
    window: Vec<bool>,
    cap: usize,
}
impl DriftStats {
    fn new(cap: usize) -> Self {
        Self {
            window: Vec::with_capacity(cap),
            cap,
        }
    }
    fn record(&mut self, ok: bool) {
        if self.window.len() == self.cap {
            self.window.remove(0);
        }
        self.window.push(ok);
    }
    fn acceptance_pct(&self) -> f64 {
        if self.window.is_empty() {
            return 100.0;
        }
        let ok = self.window.iter().filter(|b| **b).count();
        (ok as f64) * 100.0 / (self.window.len() as f64)
    }
}

impl RpcAccountFetcher {
    /// Construct with an explicit `max_slot_drift` (in slots).
    /// A sensible Devnet default is ~150 slots.
    pub fn new_with_drift(
        rpc: Arc<RpcClient>,
        commitment: CommitmentConfig,
        max_slot_drift: u64,
    ) -> Self {
        Self {
            rpc,
            commitment,
            max_slot_drift,
            stats: Arc::new(Mutex::new(DriftStats::new(256))),
        }
    }

    /// Convenience: default to 150 slots max drift.
    pub fn new(rpc: Arc<RpcClient>, commitment: CommitmentConfig) -> Self {
        Self::new_with_drift(rpc, commitment, 150)
    }
}

impl AccountFetcher for RpcAccountFetcher {
    fn fetch_many(&self, keys: &[Pubkey]) -> Result<HashMap<Pubkey, FetchedAccount>, OccError> {
        use solana_client::rpc_response::Response as RpcResponse;

        // Capture latest slot at the same commitment.
        let latest_slot = self
            .rpc
            .get_slot_with_commitment(self.commitment)
            .map_err(|e| OccError::Rpc(format!("get_slot: {e}")))?;

        // Fetch accounts (one response slot for the whole batch).
        let RpcResponse { context, value } = self
            .rpc
            .get_multiple_accounts_with_commitment(keys, self.commitment)
            .map_err(|e| OccError::Rpc(format!("get_multiple_accounts: {e}")))?;

        let resp_slot = context.slot;
        let drift = latest_slot.saturating_sub(resp_slot);
        let ok = drift <= self.max_slot_drift;

        // Track acceptance over a sliding window; warn if < 98%.
        {
            let (pct, window_full) = if let Ok(mut s) = self.stats.lock() {
                s.record(ok);
                (s.acceptance_pct(), s.window.len() == s.cap)
            } else {
                tracing::warn!(target: "occ", "drift stats mutex poisoned; skipping update");
                (100.0, false)
            };
            // Log occasionally when the window is full or on any failure.
            if !ok || window_full {
                tracing::info!(
                    target: "occ",
                    latest_slot = latest_slot,
                    response_slot = resp_slot,
                    drift = drift,
                    max_slot_drift = self.max_slot_drift,
                    acceptance_pct = pct,
                    "OCC slot-drift check: ok={}, acceptance={:.2}%",
                    ok,
                    pct
                );
                if pct < 98.0 {
                    tracing::warn!(
                        target: "occ",
                        acceptance_pct = pct,
                        "OCC slot-drift acceptance below 98% threshold"
                    );
                }
            }
        }

        // Build output snapshot (stamp every account with the response slot).
        let mut out = HashMap::with_capacity(keys.len());
        for (i, acc_opt) in value.into_iter().enumerate() {
            let key = keys[i];
            let acc = acc_opt.ok_or(OccError::MissingAccount(key))?;
            out.insert(
                key,
                FetchedAccount {
                    key,
                    lamports: acc.lamports,
                    owner: acc.owner,
                    data: acc.data,
                    slot: resp_slot,
                },
            );
        }
        Ok(out)
    }
}
