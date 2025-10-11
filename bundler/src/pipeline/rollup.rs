use std::{
    cmp,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use rand::{thread_rng, Rng};
use tokio::{sync::Semaphore, task, time::sleep};

use cpsr_types::UserIntent;
use estimator::config::SafetyMargins;
use solana_sdk::signer::Signer;
use tracing::{debug, error, info, warn};

use crate::{
    alt::{AltManager, AltResolution},
    occ::{AccountFetcher, OccConfig, OccError, OccSnapshot, capture_occ_with_retries},
    pipeline::blockhash::{BlockhashLease, BlockhashProvider, LeaseOutcome},
    sender::{ReliableSender, SendReport},
    serializer::{BuildTxCtx, build_v0_message},
    fee::FeePlan,
};

/// Planner estimates attached to a chunk (populated upstream).
#[derive(Clone, Default, Debug)]
pub struct ChunkEstimates {
    pub est_cu: u64,
    pub est_msg_bytes: usize,
    pub alt_ro_count: usize,
    pub alt_wr_count: usize,
}

/// Fully materialised chunk ready for send attempts.
#[derive(Clone)]
pub struct ChunkPlan {
    pub index: usize,
    pub intents: Vec<UserIntent>,
    pub estimates: ChunkEstimates,
}

/// Shared dependencies required by the parallel sender.
pub struct ParallelLayerDeps<S, F>
where
    S: Signer + Send + Sync + 'static,
    F: AccountFetcher + Send + Sync + 'static,
{
    pub sender: Arc<ReliableSender>,
    pub payer: Arc<S>,
    pub safety: SafetyMargins,
    pub alt_manager: Arc<dyn AltManager>,
    pub occ_fetcher: Arc<F>,
    pub occ_config: OccConfig,
    pub blockhash_provider: Arc<dyn BlockhashProvider>,
}

impl<S, F> Clone for ParallelLayerDeps<S, F>
where
    S: Signer + Send + Sync + 'static,
    F: AccountFetcher + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            payer: self.payer.clone(),
            safety: self.safety.clone(),
            alt_manager: self.alt_manager.clone(),
            occ_fetcher: self.occ_fetcher.clone(),
            occ_config: self.occ_config.clone(),
            blockhash_provider: self.blockhash_provider.clone(),
        }
    }
}

/// Runtime knobs for the parallel layer execution.
#[derive(Clone, Debug)]
pub struct ParallelLayerConfig {
    pub max_concurrency: usize,
    pub max_attempts: usize,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    pub dry_run: bool,
}

impl ParallelLayerConfig {
    pub fn sanitised(&self) -> Self {
        let max_concurrency = self.max_concurrency.clamp(1, 64);
        let max_attempts = self.max_attempts.clamp(1, 8);
        let base_backoff = if self.base_backoff.is_zero() {
            Duration::from_millis(150)
        } else {
            self.base_backoff
        };
        let max_backoff = if self.max_backoff < base_backoff {
            base_backoff
        } else {
            self.max_backoff
        };
        Self {
            max_concurrency,
            max_attempts,
            base_backoff,
            max_backoff,
            dry_run: self.dry_run,
        }
    }
}

/// Result of executing a layer under the parallel driver.
pub struct LayerResult {
    pub layer_index: usize,
    pub wall_clock: Duration,
    pub chunks: Vec<ChunkResult>,
}

/// Timings gathered for a chunk attempt (successful or failed).
#[derive(Clone, Copy, Debug, Default)]
pub struct ChunkTimings {
    pub total: Duration,
    pub occ: Option<Duration>,
    pub alt: Option<Duration>,
}

/// Successful outcome details.
pub struct ChunkSuccessData {
    pub report: SendReport,
    pub occ_snapshot: OccSnapshot,
    pub alt_resolution: AltResolution,
    pub lease: BlockhashLease,
}

/// Final per-chunk outcome.
pub struct ChunkResult {
    pub index: usize,
    pub attempts: u32,
    pub outcome: ChunkOutcome,
    pub timings: ChunkTimings,
    pub occ_accounts: usize,
    pub alt_tables: usize,
    pub estimates: ChunkEstimates,
}

pub enum ChunkOutcome {
    Success(ChunkSuccessData),
    Failed {
        error: anyhow::Error,
        retriable: bool,
        stage: AttemptStage,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum AttemptStage {
    Occ,
    AltResolution,
    Blockhash,
    Send,
}

struct ChunkAttemptError {
    stage: AttemptStage,
    error: anyhow::Error,
    retriable: bool,
    timings: ChunkTimings,
}

struct ChunkSuccess {
    data: ChunkSuccessData,
    timings: ChunkTimings,
}

/// Execute a DAG layer in parallel with bounded concurrency and outer retries.
pub async fn send_layer_parallel<S, F>(
    layer_index: usize,
    plans: Vec<ChunkPlan>,
    deps: ParallelLayerDeps<S, F>,
    cfg: ParallelLayerConfig,
) -> LayerResult
where
    S: Signer + Send + Sync + 'static,
    F: AccountFetcher + Send + Sync + 'static,
{
    if plans.is_empty() {
        return LayerResult {
            layer_index,
            wall_clock: Duration::ZERO,
            chunks: Vec::new(),
        };
    }

    let cfg = cfg.sanitised();
    let layer_start = Instant::now();
    let semaphore = Arc::new(Semaphore::new(cfg.max_concurrency));

    let mut handles = Vec::with_capacity(plans.len());
    for plan in plans {
        let idx = plan.index;
        let deps_cloned = deps.clone();
        let cfg_cloned = cfg.clone();
        let sem_cloned = semaphore.clone();
        let handle = tokio::spawn(async move {
            let permit = sem_cloned.acquire_owned().await.expect("layer semaphore closed");
            let result = process_chunk(plan, deps_cloned, cfg_cloned).await;
            drop(permit);
            result
        });
        handles.push((idx, handle));
    }

    let mut results: Vec<ChunkResult> = Vec::with_capacity(handles.len());
    for (idx, handle) in handles {
        match handle.await {
            Ok(res) => results.push(res),
            Err(join_err) => {
                error!(
                    target: "pipeline",
                    chunk_index = idx,
                    "parallel chunk task panicked: {join_err:?}"
                );
                results.push(ChunkResult {
                    index: idx,
                    attempts: 0,
                    outcome: ChunkOutcome::Failed {
                        error: anyhow!("parallel worker panicked: {join_err}"),
                        retriable: false,
                        stage: AttemptStage::Send,
                    },
                    timings: ChunkTimings::default(),
                    occ_accounts: 0,
                    alt_tables: 0,
                    estimates: ChunkEstimates::default(),
                });
            }
        }
    }

    results.sort_by_key(|r| r.index);

    LayerResult {
        layer_index,
        wall_clock: layer_start.elapsed(),
        chunks: results,
    }
}

async fn process_chunk<S, F>(
    plan: ChunkPlan,
    deps: ParallelLayerDeps<S, F>,
    cfg: ParallelLayerConfig,
) -> ChunkResult
where
    S: Signer + Send + Sync + 'static,
    F: AccountFetcher + Send + Sync + 'static,
{
    let mut attempt = 0u32;
    let mut backoff = cfg.base_backoff;

    loop {
        attempt += 1;
        info!(
            target: "pipeline",
            chunk_index = plan.index,
            attempt,
            intents = plan.intents.len(),
            "parallel chunk attempt started"
        );

        match process_chunk_once(&plan, &deps, &cfg).await {
            Ok(success) => {
                // (nice) expose a sample of ALT tables used for observability
                if !success.data.alt_resolution.tables.is_empty() {
                    let sample: Vec<String> = success
                        .data
                        .alt_resolution
                        .tables
                        .iter()
                        .take(3)
                        .map(|t| t.key.to_string())
                        .collect();
                    debug!(target: "pipeline", chunk_index = plan.index, alt_tables = ?sample, "alt tables used (sample)");
                }

                info!(
                    target: "pipeline",
                    chunk_index = plan.index,
                    attempt,
                    total_ms = success.timings.total.as_millis(),
                    simulate_ms = success
                        .data
                        .report
                        .simulate_duration
                        .as_millis(),
                    send_ms = success
                        .data
                        .report
                        .send_duration
                        .map(|d| d.as_millis())
                        .unwrap_or(0),
                    "parallel chunk succeeded"
                );
                return chunk_success_result(plan, attempt, success);
            }
            Err(err) => {
                warn!(
                    target: "pipeline",
                    chunk_index = plan.index,
                    attempt,
                    stage = ?err.stage,
                    retriable = err.retriable,
                    "parallel chunk attempt failed: {:#}",
                    err.error
                );
                if !err.retriable || attempt >= cfg.max_attempts as u32 {
                    return chunk_failure_result(plan, attempt, err);
                }

                // Exponential backoff with jitter.
                let base_ms = backoff.as_millis().max(1) as u64;
                let jitter_ms = thread_rng().gen_range(0..=base_ms / 4);
                let sleep_for = Duration::from_millis(
                    cmp::min(base_ms + jitter_ms, cfg.max_backoff.as_millis() as u64),
                );
                debug!(
                    target: "pipeline",
                    chunk_index = plan.index,
                    attempt,
                    backoff_ms = sleep_for.as_millis(),
                    "sleeping before retry"
                );
                sleep(sleep_for).await;
                backoff = cmp::min(cfg.max_backoff, backoff.saturating_mul(2));
            }
        }
    }
}

async fn process_chunk_once<S, F>(
    plan: &ChunkPlan,
    deps: &ParallelLayerDeps<S, F>,
    cfg: &ParallelLayerConfig,
) -> Result<ChunkSuccess, ChunkAttemptError>
where
    S: Signer + Send + Sync + 'static,
    F: AccountFetcher + Send + Sync + 'static,
{
    let total_start = Instant::now();

    let intents_arc = Arc::new(plan.intents.clone());
    let occ_fetcher = deps.occ_fetcher.clone();
    let occ_cfg = deps.occ_config.clone();

    let occ_start = Instant::now();
    let occ_snapshot = task::spawn_blocking({
        let intents = intents_arc.clone();
        move || capture_occ_with_retries(occ_fetcher.as_ref(), intents.as_ref(), &occ_cfg)
    })
    .await
    .map_err(|e| ChunkAttemptError {
        stage: AttemptStage::Occ,
        error: anyhow!("occ task join error: {e}"),
        retriable: true,
        timings: ChunkTimings {
            total: total_start.elapsed(),
            occ: None,
            alt: None,
        },
    })?
    .map_err(|e| occ_error_to_chunk_error(e, occ_start.elapsed(), total_start.elapsed()))?;
    let occ_duration = occ_start.elapsed();

    let payer_pubkey = deps.payer.pubkey();
    let alt_start = Instant::now();
    let alt_resolution = deps
        .alt_manager
        .resolve_tables(payer_pubkey, intents_arc.as_ref());
    let alt_duration = alt_start.elapsed();

    let lease = match deps.blockhash_provider.lease() {
        Ok(lease) => lease,
        Err(e) => {
            let retriable = is_transient_error(&e);
            let timings = ChunkTimings {
                total: total_start.elapsed(),
                occ: Some(occ_duration),
                alt: Some(alt_duration),
            };

            return Err(ChunkAttemptError {
                stage: AttemptStage::Blockhash,
                retriable,
                error: e,
                timings,
            });
        }
    };

    let safety = deps.safety.clone();
    let alts_for_build = alt_resolution.clone();
    let intents_for_build = intents_arc.clone();
    let lease_for_build = lease;

    let build_closure = move |plan: FeePlan| {
        let ctx = BuildTxCtx {
            payer: payer_pubkey,
            blockhash: lease_for_build.hash,
            fee: plan,
            alts: alts_for_build.clone(),
            safety: safety.clone(),
        };
        build_v0_message(&ctx, intents_for_build.as_ref())
    };

    let signers: Vec<Arc<S>> = vec![deps.payer.clone()];
    let send_result = deps
        .sender
        .clone()
        .simulate_build_and_send_with_report_async(build_closure, signers, cfg.dry_run)
        .await;

    match send_result {
        Ok(report) => {
            deps.blockhash_provider
                .record_outcome(&lease, LeaseOutcome::Success);
            Ok(ChunkSuccess {
                data: ChunkSuccessData {
                    report,
                    occ_snapshot,
                    alt_resolution,
                    lease,
                },
                timings: ChunkTimings {
                    total: total_start.elapsed(),
                    occ: Some(occ_duration),
                    alt: Some(alt_duration),
                },
            })
        }
        Err(err) => {
            if is_stale_blockhash_error(&err) {
                deps.blockhash_provider
                    .record_outcome(&lease, LeaseOutcome::StaleDetected);
            }
            Err(ChunkAttemptError {
                stage: AttemptStage::Send,
                retriable: is_transient_error(&err) || is_stale_blockhash_error(&err),
                error: err,
                timings: ChunkTimings {
                    total: total_start.elapsed(),
                    occ: Some(occ_duration),
                    alt: Some(alt_duration),
                },
            })
        }
    }
}

fn chunk_success_result(
    plan: ChunkPlan,
    attempts: u32,
    success: ChunkSuccess,
) -> ChunkResult {
    ChunkResult {
        index: plan.index,
        attempts,
        occ_accounts: success.data.occ_snapshot.versions.len(),
        alt_tables: success.data.alt_resolution.tables.len(),
        timings: success.timings,
        outcome: ChunkOutcome::Success(success.data),
        estimates: plan.estimates,
    }
}

fn chunk_failure_result(
    plan: ChunkPlan,
    attempts: u32,
    err: ChunkAttemptError,
) -> ChunkResult {
    ChunkResult {
        index: plan.index,
        attempts,
        occ_accounts: 0,
        alt_tables: 0,
        timings: err.timings,
        outcome: ChunkOutcome::Failed {
            error: err.error,
            retriable: err.retriable,
            stage: err.stage,
        },
        estimates: plan.estimates,
    }
}

fn occ_error_to_chunk_error(
    err: OccError,
    occ_duration: Duration,
    total_duration: Duration,
) -> ChunkAttemptError {
    let retriable = !err.is_fatal();
    ChunkAttemptError {
        stage: AttemptStage::Occ,
        error: anyhow!(err),
        retriable,
        timings: ChunkTimings {
            total: total_duration,
            occ: Some(occ_duration),
            alt: None,
        },
    }
}

fn is_transient_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();

    // Common network / RPC transient markers
    if  msg.contains("timeout")
        || msg.contains("timed out")
        || msg.contains("deadline")
        || msg.contains("connection reset")
        || msg.contains("connection refused")
        || msg.contains("broken pipe")
        || msg.contains("rate limit")
        || msg.contains("overloaded")
        || msg.contains("temporar")
        || msg.contains("unavailable")
        || msg.contains("http2")
        || msg.contains("gateway")
        || msg.contains("network")
    {
        return true;
    }

    // Coarse HTTP codes that are typically retriable
    let has_5xx = ["500", "502", "503", "504"]
        .iter()
        .any(|code| msg.contains(code));
    has_5xx
}

fn is_stale_blockhash_error(err: &anyhow::Error) -> bool {
    err.to_string()
        .to_ascii_lowercase()
        .contains("stale blockhash")
}
