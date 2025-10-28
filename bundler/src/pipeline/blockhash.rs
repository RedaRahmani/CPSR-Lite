use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use cpsr_types::UserIntent;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, hash::Hash, pubkey::Pubkey};
use tracing::{info, warn};

use crate::{
    er::bhfa::{collect_writables_or_payer, get_blockhash_for_accounts, BhfaConfig},
    sender::{track_get_latest_blockhash, track_is_blockhash_valid},
};

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

#[async_trait]
pub trait BlockhashProvider: Send + Sync {
    async fn lease(&self) -> Result<BlockhashLease>;
    async fn lease_for_intents(&self, intents: &[UserIntent]) -> Result<BlockhashLease> {
        let _ = intents;
        self.lease().await
    }
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
    /// Coalesces concurrent refresh attempts so only one caller hits RPC at a time.
    refresh_in_flight: AtomicBool,
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
            state: Mutex::new(BlockhashInner {
                current: None,
                pending_refresh: Some(RefreshReason::Initial),
            }),
            metrics: Mutex::new(BlockhashMetrics::default()),
            refresh_in_flight: AtomicBool::new(false),
        }
    }

    /// Perform an RPC fetch for a fresh blockhash. Network happens outside locks.
    fn fetch_lease(&self, reason: RefreshReason) -> Result<BlockhashLease> {
        track_get_latest_blockhash();
        let now = Instant::now();
        let (hash, last_valid_block_height) = self
            .rpc
            .get_latest_blockhash_with_commitment(self.commitment)
            .map_err(|e| anyhow!("getLatestBlockhash: {e}"))?;
        Ok(BlockhashLease {
            hash,
            last_valid_block_height,
            fetched_at: now,
            refresh_reason: reason,
        })
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
        let mut metrics = match self.metrics.lock() {
            Ok(m) => m,
            Err(_) => {
                tracing::warn!(target: "blockhash", "metrics mutex poisoned in bump_refresh_metric");
                return;
            }
        };
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

#[async_trait]
impl BlockhashProvider for BlockhashManager {
    async fn lease(&self) -> Result<BlockhashLease> {
        loop {
            // Determine whether a refresh is needed (brief critical section)
            let need_refresh: Option<RefreshReason> = if let Ok(mut inner) = self.state.lock() {
                if let Some(reason) = inner.pending_refresh.take() {
                    Some(reason)
                } else if let Some(state) = inner.current.as_ref() {
                    self.check_policy(state)
                } else {
                    Some(RefreshReason::Initial)
                }
            } else {
                // If mutex is poisoned, try to refresh to recover.
                Some(RefreshReason::Initial)
            };

            if let Some(reason) = need_refresh {
                // Coalesce concurrent refreshers.
                if self
                    .refresh_in_flight
                    .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
                    .is_ok()
                {
                    // We perform the fetch without holding the state lock.
                    match self.fetch_lease(reason) {
                        Ok(lease) => {
                            self.bump_refresh_metric(reason);
                            self.log_refresh(&lease, reason);
                            if let Ok(mut inner) = self.state.lock() {
                                inner.current = Some(BlockhashState {
                                    lease,
                                    uses_since_refresh: 0,
                                });
                                inner.pending_refresh = None;
                            } else {
                                tracing::warn!(target: "blockhash", "state mutex poisoned while updating after refresh");
                            }
                        }
                        Err(e) => {
                            // Allow other waiters to retry.
                            self.refresh_in_flight.store(false, AtomicOrdering::Release);
                            return Err(e);
                        }
                    }
                    // Release refresh flag so others can proceed.
                    self.refresh_in_flight.store(false, AtomicOrdering::Release);
                } else {
                    // Another task is already refreshing; wait briefly and retry.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                continue;
            }

            // No refresh needed; issue a lease and optionally pre-check.
            let (lease, needs_check) = {
                if let Ok(mut inner) = self.state.lock() {
                    if inner.current.is_none() {
                        // Defensive: trigger refresh path on next loop
                        inner.pending_refresh = Some(RefreshReason::Initial);
                        (BlockhashLease {
                            hash: Hash::default(),
                            last_valid_block_height: 0,
                            fetched_at: Instant::now(),
                            refresh_reason: RefreshReason::Initial,
                        }, false)
                    } else {
                        let state = inner.current.as_mut().expect("guarded by is_none check");
                        state.uses_since_refresh = state.uses_since_refresh.saturating_add(1);
                        let lease = state.lease;
                        let needs_check = self.pre_check && state.uses_since_refresh > 1;
                        (lease, needs_check)
                    }
                } else {
                    // If poisoned, force a refresh on next loop
                    let mut inner = self.state.lock().ok();
                    if let Some(ref mut g) = inner {
                        g.pending_refresh = Some(RefreshReason::Initial);
                    }
                    continue;
                }
            };

            if needs_check {
                track_is_blockhash_valid();
                match self
                    .rpc
                    .is_blockhash_valid(&lease.hash, self.commitment)
                {
                    Ok(true) => {
                        if let Ok(mut m) = self.metrics.lock() {
                            m.leases_issued += 1;
                        }
                        return Ok(lease);
                    }
                    Ok(false) => {
                        // Gentle anti-spin: avoid a tight loop against flaky RPCs.
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        warn!(target: "blockhash", "blockhash invalidated; fetching a new one");
                        if let Ok(mut inner) = self.state.lock() {
                            inner.current = None;
                            inner.pending_refresh = Some(RefreshReason::ValidationFailed);
                        } else {
                            tracing::warn!(target: "blockhash", "state mutex poisoned while setting validation failed; will retry");
                        }
                        continue;
                    }
                    Err(err) => return Err(anyhow!("isBlockhashValid: {err}")),
                }
            } else {
                if let Ok(mut m) = self.metrics.lock() {
                    m.leases_issued += 1;
                }
                return Ok(lease);
            }
        }
    }

    fn record_outcome(&self, lease: &BlockhashLease, outcome: LeaseOutcome) {
        if matches!(outcome, LeaseOutcome::StaleDetected) {
            if let Ok(mut m) = self.metrics.lock() {
                m.stale_detected += 1;
            }
            if let Ok(mut inner) = self.state.lock() {
                inner.current = None;
                inner.pending_refresh = Some(RefreshReason::Manual);
            }
            warn!(target: "blockhash", hash = %lease.hash, "stale blockhash detected; forcing refresh");
        }
    }

    fn mark_layer_boundary(&self) {
        if let Ok(mut inner) = self.state.lock() {
            inner.pending_refresh = Some(RefreshReason::LayerBoundary);
        }
    }

    fn metrics(&self) -> BlockhashMetrics {
        self
            .metrics
            .lock()
            .ok()
            .map(|guard| (*guard).clone())
            .unwrap_or_default()
    }
}

/// Blockhash provider that attempts Magic Router BHFA before falling back to the baseline manager.
pub struct ErAwareBlockhashProvider {
    inner: Arc<BlockhashManager>,
    cfg: BhfaConfig,
    payer: Pubkey,
}

impl ErAwareBlockhashProvider {
    pub fn new(inner: Arc<BlockhashManager>, cfg: BhfaConfig, payer: Pubkey) -> Self {
        Self { inner, cfg, payer }
    }
}

#[async_trait]
impl BlockhashProvider for ErAwareBlockhashProvider {
    async fn lease(&self) -> Result<BlockhashLease> {
        self.inner.lease().await
    }

    async fn lease_for_intents(&self, intents: &[UserIntent]) -> Result<BlockhashLease> {
        let accounts = collect_writables_or_payer(intents, self.payer);
        match get_blockhash_for_accounts(&self.cfg, &accounts).await {
            Ok((hash, last_valid_block_height)) => {
                info!(
                    target: "blockhash",
                    source = "BHFA",
                    accounts = %accounts.len(),
                    "using ER-aware blockhash"
                );
                Ok(BlockhashLease {
                    hash,
                    last_valid_block_height,
                    fetched_at: Instant::now(),
                    refresh_reason: RefreshReason::Manual,
                })
            }
            Err(err) => {
                warn!(
                    target: "blockhash",
                    "BHFA failed; falling back to L1: {:#}",
                    err
                );
                self.inner.lease_for_intents(intents).await
            }
        }
    }

    fn record_outcome(&self, lease: &BlockhashLease, outcome: LeaseOutcome) {
        self.inner.record_outcome(lease, outcome);
    }

    fn mark_layer_boundary(&self) {
        self.inner.mark_layer_boundary();
    }

    fn metrics(&self) -> BlockhashMetrics {
        self.inner.metrics()
    }
}
