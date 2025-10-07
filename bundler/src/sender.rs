// bundler/src/sender.rs
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Result, anyhow};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcSendTransactionConfig, RpcSimulateTransactionConfig},
};
use solana_sdk::{
    commitment_config::{CommitmentConfig, CommitmentLevel},
    message::VersionedMessage,
    signature::Signature,
    signer::Signer,
    transaction::VersionedTransaction,
};
use estimator::cost::apply_safety_with_cap;
use crate::fee::{FeeOracle, FeePlan};

// Global RPC metrics counters
static RPC_SIMULATE_COUNT: AtomicU64 = AtomicU64::new(0);
static RPC_IS_BLOCKHASH_VALID_COUNT: AtomicU64 = AtomicU64::new(0);
static RPC_GET_LATEST_BLOCKHASH_COUNT: AtomicU64 = AtomicU64::new(0);
static RPC_SEND_TRANSACTION_COUNT: AtomicU64 = AtomicU64::new(0);
static RPC_GET_RECENT_FEES_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, serde::Serialize)]
pub struct RpcMetrics {
    pub simulate: u64,
    pub is_blockhash_valid: u64,
    pub get_latest_blockhash: u64,
    pub send_transaction: u64,
    pub get_recent_prioritization_fees: u64,
}

pub fn global_rpc_metrics_snapshot() -> RpcMetrics {
    RpcMetrics {
        simulate: RPC_SIMULATE_COUNT.load(Ordering::Relaxed),
        is_blockhash_valid: RPC_IS_BLOCKHASH_VALID_COUNT.load(Ordering::Relaxed),
        get_latest_blockhash: RPC_GET_LATEST_BLOCKHASH_COUNT.load(Ordering::Relaxed),
        send_transaction: RPC_SEND_TRANSACTION_COUNT.load(Ordering::Relaxed),
        get_recent_prioritization_fees: RPC_GET_RECENT_FEES_COUNT.load(Ordering::Relaxed),
    }
}

// Helper functions to track external RPC calls
pub fn track_get_latest_blockhash() {
    RPC_GET_LATEST_BLOCKHASH_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn track_is_blockhash_valid() {
    RPC_IS_BLOCKHASH_VALID_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn track_get_recent_prioritization_fees() {
    RPC_GET_RECENT_FEES_COUNT.fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone, Debug)]
pub struct SendReport {
    pub signature: Option<Signature>,       // None in dry-run
    pub used_cu: u32,                       // from simulation
    pub final_plan: FeePlan,                // cu_limit + price used to build the final msg
    pub message_bytes: usize,               // serialized vmsg size (no signatures)
    pub required_signers: usize,            // from message header
    pub base_fee_lamports: u64,             // 5000 * required_signers
    pub priority_fee_lamports: u64,         // (cu_limit * cu_price_microLam) / 1_000_000
    pub total_fee_lamports: u64,            // base + priority (excludes rent, etc.)
}

/// Maintains a sliding window of recent used CU to compute P95.
#[derive(Default)]
struct CuStats {
    window: VecDeque<u32>,
    cap: usize, // max samples to keep
}
impl CuStats {
    fn new(cap: usize) -> Self { Self { window: VecDeque::with_capacity(cap), cap } }
    fn clear(&mut self) { self.window.clear(); }
    fn record(&mut self, used: u32) {
        if self.window.len() == self.cap { self.window.pop_front(); }
        self.window.push_back(used);
    }
    fn p95(&self) -> Option<u32> {
        if self.window.is_empty() { return None; }
        let mut v: Vec<u32> = self.window.iter().copied().collect();
        v.sort_unstable();
        let n = v.len();
        let idx = (((n as f64) * 0.95).ceil() as usize).saturating_sub(1).min(n - 1);
        Some(v[idx])
    }
}

/// How to scope the CU window when computing P95.
#[derive(Clone, Copy, Debug)]
pub enum CuScope {
    /// Learn one window for the entire process (default).
    Global,
    /// Caller resets between scopes (e.g., per DAG layer).
    PerScope,
}

pub struct ReliableSender {
    pub rpc: Arc<RpcClient>,
    pub fee_oracle: Box<dyn FeeOracle>,
    stats: Mutex<CuStats>,
    scope: CuScope,
}

impl ReliableSender {
    /// Default: global P95 window (good for steady workloads).
    pub fn new(rpc: Arc<RpcClient>, fee_oracle: Box<dyn FeeOracle>) -> Self {
        Self { rpc, fee_oracle, stats: Mutex::new(CuStats::new(128)), scope: CuScope::Global }
    }

    /// Configure the CU scope policy.
    pub fn with_scope(rpc: Arc<RpcClient>, fee_oracle: Box<dyn FeeOracle>, scope: CuScope) -> Self {
        Self { rpc, fee_oracle, stats: Mutex::new(CuStats::new(128)), scope }
    }

    /// If scope == PerScope, call this at the start of each logical scope (e.g., each DAG layer).
    pub fn start_scope(&self) {
        if let CuScope::PerScope = self.scope {
            if let Ok(mut s) = self.stats.lock() { s.clear(); }
        }
    }

    /// Simulate -> tighten CU limit with P95+Safety -> rebuild -> (optionally) send.
    /// Returns a rich SendReport for cost accounting.
    pub fn simulate_build_and_send_with_report<FBuild>(
        &self,
        mut build: FBuild,
        signers: &[&dyn Signer],
        dry_run: bool,
    ) -> Result<SendReport>
    where
        FBuild: FnMut(FeePlan) -> Result<VersionedMessage>,
    {
        // 1) Start with oracle suggestion (clamped).
        let plan0 = self.fee_oracle.clamp(self.fee_oracle.suggest());

        // 2) Build & simulate to learn units.
        let vmsg0 = build(plan0.clone())?;
        let tx0 = VersionedTransaction::try_new(vmsg0.clone(), signers)?;
        
        let sim_start = Instant::now();
        let sim = self.rpc.simulate_transaction_with_config(
            &tx0,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: true,
                ..Default::default()
            },
        )?;
        let sim_duration = sim_start.elapsed();
        RPC_SIMULATE_COUNT.fetch_add(1, Ordering::Relaxed);
        tracing::info!("simulate_transaction took {:?}", sim_duration);
        if let Some(err) = sim.value.err {
            return Err(anyhow!("simulation error: {:?}", err));
        }
        let used = sim.value.units_consumed.unwrap_or(200_000) as u32;

        // 3) Update stats and compute P95 envelope.
        {
            let mut s = self.stats.lock().expect("cu stats mutex");
            s.record(used);
        }
        let p95_used = self.stats.lock().unwrap().p95().unwrap_or(used);
        let target_limit = p95_used.max(used); // conservative

        // 4) Apply safety (+10k +20%, clamped to oracle’s max limit).
        let safe = apply_safety_with_cap(target_limit, 10_000, 20, plan0.cu_limit as u32);
        let mut plan = FeePlan {
            cu_limit: safe.cu_with_safety as u64,
            cu_price_microlamports: plan0.cu_price_microlamports,
        };
        plan = self.fee_oracle.clamp(plan);

        // 5) Rebuild with final plan.
        let vmsg = build(plan.clone())?;

        // 6) Fee accounting.
        let message_bytes = bincode::serialize(&vmsg)?.len();
        let required_signers = match &vmsg {
            VersionedMessage::Legacy(m) => m.header.num_required_signatures as usize,
            VersionedMessage::V0(m) => m.header.num_required_signatures as usize,
        };
        // Base fee: 5000 lamports per signature (runtime constant).
        let base_fee_lamports = 5000u64 * (required_signers as u64);
        // Priority fee: limit * price (μLam) -> lamports
        let priority_fee_lamports = (plan.cu_limit.saturating_mul(plan.cu_price_microlamports)) / 1_000_000;
        let mut signature = None;

        if !dry_run {
            // 7) Verify blockhash then send.
            let bh = match &vmsg {
                VersionedMessage::Legacy(m) => m.recent_blockhash,
                VersionedMessage::V0(m) => m.recent_blockhash,
            };
            
            let blockhash_start = Instant::now();
            let ok = self.rpc.is_blockhash_valid(&bh, CommitmentConfig::processed())?;
            let blockhash_duration = blockhash_start.elapsed();
            RPC_IS_BLOCKHASH_VALID_COUNT.fetch_add(1, Ordering::Relaxed);
            tracing::info!("is_blockhash_valid took {:?}", blockhash_duration);
            
            if !ok {
                return Err(anyhow!("stale blockhash; rebuild required"));
            }
            let tx_final = VersionedTransaction::try_new(vmsg, signers)?;
            
            let send_start = Instant::now();
            let sig = self.rpc.send_transaction_with_config(
                &tx_final,
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    ..Default::default()
                },
            )?;
            let send_duration = send_start.elapsed();
            RPC_SEND_TRANSACTION_COUNT.fetch_add(1, Ordering::Relaxed);
            tracing::info!("send_transaction took {:?}", send_duration);
            signature = Some(sig);
        }

        let total_fee_lamports = base_fee_lamports + priority_fee_lamports;
        Ok(SendReport {
            signature,
            used_cu: used,
            final_plan: plan,
            message_bytes,
            required_signers,
            base_fee_lamports,
            priority_fee_lamports,
            total_fee_lamports,
        })
    }

    /// Backwards-compatible wrapper (returns only Signature like before).
    pub fn simulate_and_send<FBuild>(&self, build: FBuild, signers: &[&dyn Signer]) -> Result<Signature>
    where
        FBuild: FnMut(FeePlan) -> Result<VersionedMessage>,
    {
        let rep = self.simulate_build_and_send_with_report(build, signers, false)?;
        Ok(rep.signature.expect("signature missing"))
    }
}
