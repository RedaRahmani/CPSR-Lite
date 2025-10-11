use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, hash::Hash};
use tracing::{info, warn};

use crate::sender::{track_get_latest_blockhash, track_is_blockhash_valid};

/// Why a blockhash refresh occurred.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshReason {
    Initial,
    Manual,
    AgeExceeded,
    Quota,
    LayerBoundary,
    ValidationFailed,
}

/// Lease returned to callers.
#[derive(Clone, Copy, Debug)]
pub struct BlockhashLease {
    pub hash: Hash,
    pub last_valid_block_height: u64,
    pub fetched_at: Instant,
    pub refresh_reason: RefreshReason,
}

/// Result of using a lease.
#[derive(Clone, Copy, Debug)]
pub enum LeaseOutcome {
    Success,
    StaleDetected,
}

/// Policy knobs for blockhash refresh.
#[derive(Clone, Debug)]
pub struct BlockhashPolicy {
    pub max_age: Duration,
    pub refresh_every: u32,
}

impl Default for BlockhashPolicy {
    fn default() -> Self {
        Self {
            max_age: Duration::from_millis(8_000),
            refresh_every: 16,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct BlockhashMetrics {
    pub refresh_initial: u64,
    pub refresh_manual: u64,
    pub refresh_age: u64,
    pub refresh_quota: u64,
    pub refresh_layer: u64,
    pub refresh_validation: u64,
    pub leases_issued: u64,
    pub stale_detected: u64,
}

pub trait BlockhashProvider: Send + Sync {
    fn lease(&self) -> Result<BlockhashLease>;
    fn record_outcome(&self, lease: &BlockhashLease, outcome: LeaseOutcome);
    fn mark_layer_boundary(&self);
    fn metrics(&self) -> BlockhashMetrics;
}

struct BlockhashState {
    lease: BlockhashLease,
    uses_since_refresh: u32,
}

struct BlockhashInner {
    current: Option<BlockhashState>,
    pending_refresh: Option<RefreshReason>,
}

pub struct BlockhashManager {
    rpc: Arc<RpcClient>,
    commitment: CommitmentConfig,
    policy: BlockhashPolicy,
    pre_check: bool,
    state: Mutex<BlockhashInner>,
    metrics: Mutex<BlockhashMetrics>,
}

impl BlockhashManager {
    pub fn new(
        rpc: Arc<RpcClient>,
        commitment: CommitmentConfig,
        mut policy: BlockhashPolicy,
        pre_check: bool,
    ) -> Self {
        if policy.refresh_every == 0 {
            policy.refresh_every = 1;
        }
        Self {
            rpc,
            commitment,
            policy,
            pre_check,
            state: Mutex::new(BlockhashInner { current: None, pending_refresh: Some(RefreshReason::Initial) }),
            metrics: Mutex::new(BlockhashMetrics::default()),
        }
    }

    fn refresh_with_reason(
        &self,
        inner: &mut BlockhashInner,
        reason: RefreshReason,
    ) -> Result<()> {
        track_get_latest_blockhash();
        let now = Instant::now();
        let (hash, last_valid_block_height) = self
            .rpc
            .get_latest_blockhash_with_commitment(self.commitment.clone())
            .map_err(|e| anyhow!("getLatestBlockhash: {e}"))?;

        let lease = BlockhashLease {
            hash,
            last_valid_block_height,
            fetched_at: now,
            refresh_reason: reason,
        };

        self.bump_refresh_metric(reason);
        self.log_refresh(&lease, reason);

        inner.current = Some(BlockhashState { lease, uses_since_refresh: 0 });
        inner.pending_refresh = None;

        Ok(())
    }

    fn check_policy(&self, state: &BlockhashState) -> Option<RefreshReason> {
        let now = Instant::now();
        if self.policy.max_age > Duration::ZERO
            && now.duration_since(state.lease.fetched_at) > self.policy.max_age
        {
            return Some(RefreshReason::AgeExceeded);
        }
        if state.uses_since_refresh >= self.policy.refresh_every {
            return Some(RefreshReason::Quota);
        }
        None
    }

    fn bump_refresh_metric(&self, reason: RefreshReason) {
        let mut metrics = self.metrics.lock().expect("blockhash metrics mutex");
        match reason {
            RefreshReason::Initial => metrics.refresh_initial += 1,
            RefreshReason::Manual => metrics.refresh_manual += 1,
            RefreshReason::AgeExceeded => metrics.refresh_age += 1,
            RefreshReason::Quota => metrics.refresh_quota += 1,
            RefreshReason::LayerBoundary => metrics.refresh_layer += 1,
            RefreshReason::ValidationFailed => metrics.refresh_validation += 1,
        }
    }

    fn log_refresh(&self, lease: &BlockhashLease, reason: RefreshReason) {
        info!(
            target: "blockhash",
            reason = ?reason,
            hash = %lease.hash,
            last_valid_block_height = lease.last_valid_block_height,
            "refreshed blockhash"
        );
    }
}

impl BlockhashProvider for BlockhashManager {
    fn lease(&self) -> Result<BlockhashLease> {
        loop {
            let (lease, needs_check) = {
                let mut inner = self.state.lock().expect("blockhash state mutex");

                if let Some(reason) = inner.pending_refresh.take() {
                    self.refresh_with_reason(&mut inner, reason)?;
                }

                if let Some(state) = inner.current.as_ref() {
                    if let Some(reason) = self.check_policy(state) {
                        self.refresh_with_reason(&mut inner, reason)?;
                    }
                }

                if inner.current.is_none() {
                    self.refresh_with_reason(&mut inner, RefreshReason::Initial)?;
                }

                let state = inner.current.as_mut().expect("blockhash state populated");
                state.uses_since_refresh = state.uses_since_refresh.saturating_add(1);
                let lease = state.lease;
                let needs_check = self.pre_check && state.uses_since_refresh > 1;
                (lease, needs_check)
            };

            if needs_check {
                track_is_blockhash_valid();
                match self
                    .rpc
                    .is_blockhash_valid(&lease.hash, self.commitment.clone())
                {
                    Ok(true) => {
                        self.metrics.lock().expect("blockhash metrics mutex").leases_issued += 1;
                        return Ok(lease);
                    }
                    Ok(false) => {
                        // Gentle anti-spin: avoid a tight loop against flaky RPCs.
                        std::thread::sleep(Duration::from_millis(25));
                        warn!(target: "blockhash", "blockhash invalidated; fetching a new one");
                        let mut inner = self.state.lock().expect("blockhash state mutex");
                        inner.current = None;
                        inner.pending_refresh = Some(RefreshReason::ValidationFailed);
                        continue;
                    }
                    Err(err) => return Err(anyhow!("isBlockhashValid: {err}")),
                }
            } else {
                self.metrics.lock().expect("blockhash metrics mutex").leases_issued += 1;
                return Ok(lease);
            }
        }
    }

    fn record_outcome(&self, lease: &BlockhashLease, outcome: LeaseOutcome) {
        if matches!(outcome, LeaseOutcome::StaleDetected) {
            self.metrics.lock().expect("blockhash metrics mutex").stale_detected += 1;
            let mut inner = self.state.lock().expect("blockhash state mutex");
            inner.current = None;
            inner.pending_refresh = Some(RefreshReason::Manual);
            warn!(target: "blockhash", hash = %lease.hash, "stale blockhash detected; forcing refresh");
        }
    }

    fn mark_layer_boundary(&self) {
        let mut inner = self.state.lock().expect("blockhash state mutex");
        inner.pending_refresh = Some(RefreshReason::LayerBoundary);
    }

    fn metrics(&self) -> BlockhashMetrics {
        self.metrics.lock().expect("blockhash metrics mutex").clone()
    }
}
