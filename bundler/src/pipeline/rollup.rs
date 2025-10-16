use std::{
    cmp,
    collections::{BTreeSet, HashMap},
    sync::{Arc, Once},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use rand::{thread_rng, Rng};
use tokio::{sync::Semaphore, task, time::sleep};

use cpsr_types::{hash::Hash32, intent::AccessKind, UserIntent};
use estimator::config::SafetyMargins;
use solana_program::pubkey::Pubkey;
use solana_sdk::signer::Signer;
use tracing::{debug, error, info, trace, warn};

use crate::{
    alt::{AltManager, AltResolution},
    er::{
        classify_router_error, compute_plan_fingerprint, BlockhashSource, ErBlockhashPlan,
        ErClient, ErTelemetry,
    },
    fee::FeePlan,
    occ::{
        capture_occ_with_retries, capture_occ_with_retries_on_keys, AccountFetcher, OccConfig,
        OccError, OccSnapshot,
    },
    pipeline::blockhash::{BlockhashLease, BlockhashProvider, LeaseOutcome, RefreshReason},
    sender::{ReliableSender, SendReport},
    serializer::{build_v0_message, BuildTxCtx},
};

const ER_SMALL_MESSAGE_BYTES: usize = 400;

fn build_access_map(intents: &[UserIntent]) -> HashMap<Pubkey, AccessKind> {
    let mut map = HashMap::new();
    for intent in intents {
        for access in &intent.accesses {
            map.entry(access.pubkey)
                .and_modify(|existing| {
                    if matches!(access.access, AccessKind::Writable) {
                        *existing = AccessKind::Writable;
                    }
                })
                .or_insert(access.access);
        }
    }
    map
}

fn can_merge_intents(map: &mut HashMap<Pubkey, AccessKind>, intents: &[UserIntent]) -> bool {
    for intent in intents {
        for access in &intent.accesses {
            if let Some(existing) = map.get(&access.pubkey) {
                if matches!(existing, AccessKind::Writable)
                    || matches!(access.access, AccessKind::Writable)
                {
                    return false;
                }
            }
        }
    }

    for intent in intents {
        for access in &intent.accesses {
            map.entry(access.pubkey)
                .and_modify(|existing| {
                    if matches!(access.access, AccessKind::Writable) {
                        *existing = AccessKind::Writable;
                    }
                })
                .or_insert(access.access);
        }
    }

    true
}

fn merge_small_chunk_plans(
    layer_index: usize,
    plans: Vec<ChunkPlan>,
    min_cu_threshold: u64,
    safety: &SafetyMargins,
) -> Vec<ChunkPlan> {
    if plans.is_empty() {
        return plans;
    }

    let max_cu = safety.max_chunk_cu as u64;
    let max_message_bytes = safety.max_message_bytes_with_signers(1);
    let mut merged = Vec::with_capacity(plans.len());
    let mut i = 0;

    while i < plans.len() {
        let mut head = plans[i].clone();

        if head.intents.is_empty() {
            merged.push(head);
            i += 1;
            continue;
        }

        let mut combined_intents = head.intents.clone();
        let mut combined_cu = head.estimates.est_cu;
        let mut combined_msg = head.estimates.est_msg_bytes;
        let mut combined_alt_ro = head.estimates.alt_ro_count;
        let mut combined_alt_wr = head.estimates.alt_wr_count;
        let mut merged_indices: Vec<usize> = Vec::new();

        if combined_cu < min_cu_threshold && combined_msg <= ER_SMALL_MESSAGE_BYTES {
            if let Some(actor) = head.intents.first().map(|intent| intent.actor) {
                let mut access_map = build_access_map(&combined_intents);
                let mut j = i + 1;
                while j < plans.len() {
                    let candidate = &plans[j];
                    if candidate.intents.is_empty() {
                        break;
                    }
                    if candidate.estimates.est_cu >= min_cu_threshold
                        || candidate.estimates.est_msg_bytes > ER_SMALL_MESSAGE_BYTES
                    {
                        break;
                    }
                    if candidate.intents.first().map(|intent| intent.actor) != Some(actor) {
                        break;
                    }
                    if !can_merge_intents(&mut access_map, &candidate.intents) {
                        break;
                    }

                    let new_cu = combined_cu + candidate.estimates.est_cu;
                    if new_cu > max_cu {
                        break;
                    }
                    let new_msg = combined_msg + candidate.estimates.est_msg_bytes;
                    if new_msg > max_message_bytes {
                        break;
                    }

                    combined_intents.extend(candidate.intents.iter().cloned());
                    combined_cu = new_cu;
                    combined_msg = new_msg;
                    combined_alt_ro += candidate.estimates.alt_ro_count;
                    combined_alt_wr += candidate.estimates.alt_wr_count;
                    merged_indices.push(candidate.index);
                    j += 1;
                }

                if !merged_indices.is_empty() {
                    trace!(
                        target: "er.merge",
                        layer_index,
                        first_chunk_index = head.index,
                        merged_count = merged_indices.len() + 1,
                        merged_used_cu = combined_cu,
                        merged_message_bytes = combined_msg
                    );

                    head.intents = combined_intents;
                    head.estimates.est_cu = combined_cu;
                    head.estimates.est_msg_bytes = combined_msg;
                    head.estimates.alt_ro_count = combined_alt_ro;
                    head.estimates.alt_wr_count = combined_alt_wr;
                    head.merged_indices = merged_indices;
                    merged.push(head);
                    i = j;
                    continue;
                }
            }
        }

        head.merged_indices.clear();
        merged.push(head);
        i += 1;
    }

    merged
}

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
    pub merged_indices: Vec<usize>,
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
    pub er: Option<ErExecutionCtx>,
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
            er: self.er.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ErExecutionCtx {
    enabled: bool,
    client: Arc<dyn ErClient>,
    telemetry: Arc<ErTelemetry>,
    simulate_on_failure: bool,
    min_cu_threshold: u64,
    merge_small_intents: bool,
}

#[derive(Clone)]
struct ErChunkMeta {
    plan_fingerprint: Option<Hash32>,
    blockhash_plan: Option<ErBlockhashPlan>,
    settlement_accounts: Vec<Pubkey>,
    execution_duration: Option<Duration>,
    simulated: bool,
    route_endpoint: Option<String>,
    route_cache_hit: Option<bool>,
    session_id: Option<String>,
    fallback_reason: Option<String>,
}

pub(crate) struct ErPreparedChunk {
    intents: Vec<UserIntent>,
    meta: ErChunkMeta,
}

#[derive(Clone, Debug, Default)]
pub struct ErChunkDiagnostics {
    pub er_ctx_present: bool,
    pub er_enabled: bool,
    pub attempted: bool,
    pub session_established: bool,
    pub simulated: bool,
    pub route_endpoint: Option<String>,
    pub route_cache_hit: Option<bool>,
    pub session_id: Option<String>,
    pub fallback_reason: Option<String>,
}

impl ErExecutionCtx {
    pub fn new(
        client: Arc<dyn ErClient>,
        telemetry: Arc<ErTelemetry>,
        enabled: bool,
        simulate_on_failure: bool,
        min_cu_threshold: u64,
        merge_small_intents: bool,
    ) -> Self {
        Self {
            enabled,
            client,
            telemetry,
            simulate_on_failure,
            min_cu_threshold,
            merge_small_intents,
        }
    }

    pub fn telemetry(&self) -> Arc<ErTelemetry> {
        Arc::clone(&self.telemetry)
    }

    pub fn min_cu_threshold(&self) -> u64 {
        self.min_cu_threshold
    }

    pub fn merge_small_intents(&self) -> bool {
        self.merge_small_intents
    }

    pub(crate) async fn prepare_chunk(
        &self,
        chunk_index: usize,
        payer: Pubkey,
        intents: &[UserIntent],
    ) -> Result<ErPreparedChunk> {
        if !self.enabled {
            bail_disabled()
        } else {
            self.prepare_chunk_inner(chunk_index, payer, intents).await
        }
    }

    async fn prepare_chunk_inner(
        &self,
        chunk_index: usize,
        payer: Pubkey,
        intents: &[UserIntent],
    ) -> Result<ErPreparedChunk> {
        if intents.is_empty() {
            return Err(anyhow!("no intents to execute via ER"));
        }

        let accounts = collect_session_accounts(payer, intents);
        if accounts.is_empty() {
            return Err(anyhow!("no accounts derived for ER session"));
        }

        let plan_fp = compute_plan_fingerprint(intents);

        self.telemetry.record_session_begin_attempt();
        let begin_result = self.client.begin_session(&accounts).await;

        let session = match begin_result {
            Ok(session) => {
                self.telemetry.record_session_begin_ok();
                if let Some(plan) = session.blockhash_plan.as_ref() {
                    self.telemetry.record_route(Some(plan.route_duration), true);
                    self.telemetry
                        .record_blockhash(Some(plan.blockhash_duration), true);
                    self.telemetry.record_route_cache(session.route.cache_hit);

                    let source_str = match plan.blockhash.source {
                        BlockhashSource::Router => "router",
                        BlockhashSource::RouteEndpoint => "route",
                    };
                    info!(
                        target: "er",
                        chunk_index,
                        source = source_str,
                        route = plan.route.endpoint.as_str(),
                        route_ttl_ms = plan.route.ttl.as_millis(),
                        route_ms = plan.route_duration.as_millis(),
                        bhfa_ms = plan.blockhash_duration.as_millis(),
                        lvh = plan.blockhash.last_valid_block_height,
                        cache_hit = session.route.cache_hit,
                        "ER session prepared"
                    );
                } else {
                    self.telemetry
                        .record_route(Some(session.route_duration), true);
                    self.telemetry.record_blockhash(None, true);
                    self.telemetry.record_route_cache(session.route.cache_hit);
                    info!(
                        target: "er",
                        chunk_index,
                        route = session.route.endpoint.as_str(),
                        route_ttl_ms = session.route.ttl.as_millis(),
                        route_ms = session.route_duration.as_millis(),
                        cache_hit = session.route.cache_hit,
                        "ER session prepared (no blockhash plan)"
                    );
                }
                session
            }
            Err(err) => {
                self.telemetry.record_session_begin_err();
                self.telemetry.record_last_router_error(&err);
                self.telemetry.record_route(None, false);
                self.telemetry.record_blockhash(None, false);
                self.telemetry.record_fallback();
                if let Some(kind) = classify_router_error(&err) {
                    self.telemetry.record_router_error_kind(kind);
                    warn!(
                        target: "er",
                        chunk_index,
                        error_class = kind.as_str(),
                        fallback = self.simulate_on_failure,
                        "ER begin_session failed: {:#}",
                        err
                    );
                } else {
                    warn!(
                        target: "er",
                        chunk_index,
                        fallback = self.simulate_on_failure,
                        "ER begin_session failed: {:#}",
                        err
                    );
                }
                if self.simulate_on_failure {
                    warn!(
                        target: "er",
                        chunk_index,
                        "ER simulation fallback (begin_session failed)"
                    );
                    let reason = format!("begin_session failed: {:#}", err);
                    return Ok(self.simulate_chunk(payer, intents, Some(reason)));
                }
                return Err(err);
            }
        };

        let intents_for_exec: Vec<UserIntent> = intents.to_vec();
        let exec_result = self.client.execute(&session, &intents_for_exec).await;

        let output = match exec_result {
            Ok(output) => output,
            Err(err) => {
                self.telemetry.record_fallback();
                self.telemetry.record_last_router_error(&err);
                let router_error_kind = classify_router_error(&err);
                if let Some(kind) = router_error_kind {
                    self.telemetry.record_router_error_kind(kind);
                }

                let phase1_disabled = is_phase1_disabled_error(&err);
                if phase1_disabled {
                    log_phase1_fallback_once();
                } else if let Some(kind) = router_error_kind {
                    warn!(
                        target: "er",
                        chunk_index,
                        error_class = kind.as_str(),
                        fallback = self.simulate_on_failure,
                        "ER execute failed: {:#}",
                        err
                    );
                } else {
                    warn!(
                        target: "er",
                        chunk_index,
                        fallback = self.simulate_on_failure,
                        "ER execute failed: {:#}",
                        err
                    );
                }

                // best-effort session cleanup before fallback
                if let Err(cleanup_err) = self.client.end_session(&session).await {
                    warn!(
                        target: "er",
                        chunk_index,
                        "ER cleanup after execute failure errored: {:#}",
                        cleanup_err
                    );
                }

                if self.simulate_on_failure {
                    warn!(
                        target: "er",
                        chunk_index,
                        "ER simulation fallback (execute failed)"
                    );
                    let reason = format!("execute failed: {:#}", err);
                    return Ok(self.simulate_chunk(payer, intents, Some(reason)));
                }
                return Err(err);
            }
        };

        // Session completed successfully; perform final cleanup.
        if let Err(end_err) = self.client.end_session(&session).await {
            warn!(
                target: "er",
                chunk_index,
                "ER end_session returned error: {:#}",
                end_err
            );
        }

        let settlement_accounts = if output.settlement_accounts.is_empty() {
            accounts.clone()
        } else {
            output.settlement_accounts.clone()
        };

        let settlement_plan_fp = output.plan_fingerprint.or(Some(plan_fp));
        let settlement_blockhash = output
            .blockhash_plan
            .clone()
            .or_else(|| session.blockhash_plan.clone());

        let settlement_intents: Vec<UserIntent> = output
            .settlement_instructions
            .into_iter()
            .map(|ix| UserIntent::new(payer, ix, 0, None).normalized())
            .collect();

        if settlement_intents.is_empty() {
            self.telemetry.record_fallback();
            let err = anyhow!("ER returned zero settlement instructions");
            warn!(target: "er", chunk_index, "{:#}", err);
            if self.simulate_on_failure {
                let reason = format!("execute produced no settlement instructions: {:#}", err);
                return Ok(self.simulate_chunk(payer, intents, Some(reason)));
            }
            return Err(err);
        }

        info!(
            target: "er",
            chunk_index,
            session_id = session.id.as_str(),
            settlement_ixs = settlement_intents.len(),
            settlement_accounts = settlement_accounts.len(),
            exec_ms = output.exec_duration.as_millis(),
            "ER execution produced settlement instructions"
        );

        Ok(ErPreparedChunk {
            intents: settlement_intents,
            meta: ErChunkMeta {
                plan_fingerprint: settlement_plan_fp,
                blockhash_plan: settlement_blockhash,
                settlement_accounts,
                execution_duration: Some(output.exec_duration),
                simulated: false,
                route_endpoint: Some(session.endpoint.as_str().to_string()),
                route_cache_hit: Some(session.route.cache_hit),
                session_id: Some(session.id.clone()),
                fallback_reason: None,
            },
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn simulate_chunk(
        &self,
        _payer: Pubkey,
        intents: &[UserIntent],
        reason: Option<String>,
    ) -> ErPreparedChunk {
        let plan_fp = compute_plan_fingerprint(intents);

        ErPreparedChunk {
            intents: intents.to_vec(),
            meta: ErChunkMeta {
                plan_fingerprint: Some(plan_fp),
                blockhash_plan: None,
                settlement_accounts: Vec::new(),
                execution_duration: None,
                simulated: true,
                route_endpoint: None,
                route_cache_hit: None,
                session_id: None,
                fallback_reason: reason,
            },
        }
    }
}

fn is_phase1_disabled_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        let msg = cause.to_string().to_lowercase();
        msg.contains("phase") && msg.contains("disable")
    })
}

fn log_phase1_fallback_once() {
    static PHASE1_DISABLED_ONCE: Once = Once::new();
    PHASE1_DISABLED_ONCE.call_once(|| {
        warn!(
            target: "er",
            "Magic Router indicates Phase-1 execution is disabled; falling back to local pipeline"
        );
    });
}

fn bail_disabled<T>() -> Result<T> {
    Err(anyhow!("ER disabled"))
}

fn collect_session_accounts(payer: Pubkey, intents: &[UserIntent]) -> Vec<Pubkey> {
    let mut set = BTreeSet::new();
    set.insert(payer);
    for intent in intents {
        for access in &intent.accesses {
            set.insert(access.pubkey);
        }
    }
    set.into_iter().collect()
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
    pub plan_fingerprint: Option<Hash32>,
    pub er_blockhash_plan: Option<ErBlockhashPlan>,
    pub er_simulated: bool,
    pub er_execution_duration: Option<Duration>,
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
    pub er: ErChunkDiagnostics,
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
    er_diag: ErChunkDiagnostics,
}

struct ChunkSuccess {
    data: ChunkSuccessData,
    timings: ChunkTimings,
    er_diag: ErChunkDiagnostics,
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

    let mut plans = plans;
    if let Some(er_ctx) = deps.er.as_ref() {
        if er_ctx.is_enabled() && er_ctx.merge_small_intents() {
            plans = merge_small_chunk_plans(
                layer_index,
                plans,
                er_ctx.min_cu_threshold(),
                &deps.safety,
            );
        }
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
            let permit = sem_cloned
                .acquire_owned()
                .await
                .expect("layer semaphore closed");
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
                    er: ErChunkDiagnostics::default(),
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
                    er_route_ms = success
                        .data
                        .er_blockhash_plan
                        .as_ref()
                        .map(|p| p.route_duration.as_millis()),
                    er_bhfa_ms = success
                        .data
                        .er_blockhash_plan
                        .as_ref()
                        .map(|p| p.blockhash_duration.as_millis()),
                    mode = if success.data.er_blockhash_plan.is_some() {
                        "ER"
                    } else {
                        "Baseline"
                    },
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
                let sleep_for = Duration::from_millis(cmp::min(
                    base_ms + jitter_ms,
                    cfg.max_backoff.as_millis() as u64,
                ));
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
    let payer_pubkey = deps.payer.pubkey();

    let mut prepared_intents = plan.intents.clone();
    let mut er_meta: Option<ErChunkMeta> = None;
    let er_telemetry = deps.er.as_ref().map(|ctx| ctx.telemetry());
    let mut er_diag = ErChunkDiagnostics {
        er_ctx_present: deps.er.is_some(),
        er_enabled: deps
            .er
            .as_ref()
            .map(|ctx| ctx.is_enabled())
            .unwrap_or(false),
        ..ErChunkDiagnostics::default()
    };

    if let Some(er_ctx) = &deps.er {
        er_diag.er_ctx_present = true;
        er_diag.er_enabled = er_ctx.is_enabled();
        if er_ctx.is_enabled() {
            let threshold = er_ctx.min_cu_threshold();
            let below_cu = threshold > 0 && plan.estimates.est_cu < threshold;
            let small_message = plan.estimates.est_msg_bytes <= ER_SMALL_MESSAGE_BYTES;
            if below_cu || small_message {
                let reason = if below_cu && small_message {
                    "below_threshold+small_message"
                } else if below_cu {
                    "below_threshold"
                } else {
                    "small_message"
                };
                er_diag.fallback_reason = Some(reason.to_string());
                if let Some(telemetry) = er_telemetry.as_ref() {
                    telemetry.record_small_chunk_skip();
                }
            } else {
                er_diag.attempted = true;
                if let Some(telemetry) = er_telemetry.as_ref() {
                    if !plan.merged_indices.is_empty() {
                        telemetry.record_small_chunk_merge(plan.merged_indices.len() as u64);
                    }
                }
                match er_ctx
                    .prepare_chunk(plan.index, payer_pubkey, &plan.intents)
                    .await
                {
                    Ok(prepared) => {
                        let ErPreparedChunk {
                            intents: new_intents,
                            meta,
                        } = prepared;
                        if meta.simulated {
                            warn!(
                                target: "er",
                                chunk_index = plan.index,
                                "ER session simulated (dry-run)"
                            );
                        }
                        er_meta = Some(meta);
                        er_diag.simulated = er_meta.as_ref().map(|m| m.simulated).unwrap_or(false);
                        if let Some(meta) = er_meta.as_ref() {
                            er_diag.session_established = !meta.simulated;
                            er_diag.route_endpoint = meta.route_endpoint.clone();
                            er_diag.route_cache_hit = meta.route_cache_hit;
                            er_diag.session_id = meta.session_id.clone();
                            if let Some(reason) = meta.fallback_reason.clone() {
                                er_diag.fallback_reason = Some(reason);
                            }
                        }
                        prepared_intents = new_intents;
                    }
                    Err(err) => {
                        if is_phase1_disabled_error(&err) {
                            log_phase1_fallback_once();
                        } else {
                            warn!(
                                target: "er",
                                chunk_index = plan.index,
                                "ER fallback engaged: {:#}",
                                err
                            );
                        }
                        er_diag.fallback_reason = Some(format!("{:#}", err));
                    }
                }
            }
        } else if er_diag.fallback_reason.is_none() {
            er_diag.fallback_reason = Some("er_disabled".to_string());
        }
    }

    let intents_arc = Arc::new(prepared_intents);
    let occ_fetcher = deps.occ_fetcher.clone();
    let occ_cfg = deps.occ_config.clone();

    let occ_start = Instant::now();
    let occ_accounts_override = er_meta
        .as_ref()
        .filter(|meta| !meta.simulated && !meta.settlement_accounts.is_empty())
        .map(|meta| meta.settlement_accounts.clone());

    let occ_snapshot = task::spawn_blocking({
        let intents = intents_arc.clone();
        let accounts_override = occ_accounts_override.clone();
        let occ_fetcher = occ_fetcher.clone();
        let occ_cfg = occ_cfg.clone();
        move || {
            if let Some(accounts) = accounts_override {
                capture_occ_with_retries_on_keys(occ_fetcher.as_ref(), &accounts, &occ_cfg)
            } else {
                capture_occ_with_retries(occ_fetcher.as_ref(), intents.as_ref(), &occ_cfg)
            }
        }
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
        er_diag: er_diag.clone(),
    })?
    .map_err(|e| {
        occ_error_to_chunk_error(
            e,
            occ_start.elapsed(),
            total_start.elapsed(),
            er_diag.clone(),
        )
    })?;
    let occ_duration = occ_start.elapsed();

    let alt_start = Instant::now();
    let alt_resolution = deps
        .alt_manager
        .resolve_tables(payer_pubkey, intents_arc.as_ref());
    let alt_duration = alt_start.elapsed();

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum LeaseOrigin {
        Baseline,
        Er,
    }

    let er_plan_ref = er_meta
        .as_ref()
        .filter(|meta| !meta.simulated)
        .and_then(|meta| meta.blockhash_plan.as_ref());

    let (lease, lease_origin) = if let Some(plan) = er_plan_ref {
        let lease = BlockhashLease {
            hash: plan.blockhash.hash,
            last_valid_block_height: plan.blockhash.last_valid_block_height,
            fetched_at: plan.blockhash_fetched_at,
            refresh_reason: RefreshReason::Manual,
        };
        (lease, LeaseOrigin::Er)
    } else {
        let lease_result = deps
            .blockhash_provider
            .lease_for_intents(intents_arc.as_ref())
            .await;
        match lease_result {
            Ok(lease) => (lease, LeaseOrigin::Baseline),
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
                    er_diag: er_diag.clone(),
                });
            }
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
            if matches!(lease_origin, LeaseOrigin::Baseline) {
                deps.blockhash_provider
                    .record_outcome(&lease, LeaseOutcome::Success);
            }
            Ok(ChunkSuccess {
                data: ChunkSuccessData {
                    report,
                    occ_snapshot,
                    alt_resolution,
                    lease,
                    plan_fingerprint: er_meta.as_ref().and_then(|m| m.plan_fingerprint),
                    er_blockhash_plan: er_meta.as_ref().and_then(|m| m.blockhash_plan.clone()),
                    er_simulated: er_meta.as_ref().map(|m| m.simulated).unwrap_or(false),
                    er_execution_duration: er_meta.as_ref().and_then(|m| m.execution_duration),
                },
                timings: ChunkTimings {
                    total: total_start.elapsed(),
                    occ: Some(occ_duration),
                    alt: Some(alt_duration),
                },
                er_diag: er_diag.clone(),
            })
        }
        Err(err) => {
            if is_stale_blockhash_error(&err) {
                if matches!(lease_origin, LeaseOrigin::Baseline) {
                    deps.blockhash_provider
                        .record_outcome(&lease, LeaseOutcome::StaleDetected);
                }
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
                er_diag: er_diag.clone(),
            })
        }
    }
}

fn chunk_success_result(plan: ChunkPlan, attempts: u32, success: ChunkSuccess) -> ChunkResult {
    ChunkResult {
        index: plan.index,
        attempts,
        occ_accounts: success.data.occ_snapshot.versions.len(),
        alt_tables: success.data.alt_resolution.tables.len(),
        timings: success.timings,
        outcome: ChunkOutcome::Success(success.data),
        estimates: plan.estimates,
        er: success.er_diag,
    }
}

fn chunk_failure_result(plan: ChunkPlan, attempts: u32, err: ChunkAttemptError) -> ChunkResult {
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
        er: err.er_diag,
    }
}

fn occ_error_to_chunk_error(
    err: OccError,
    occ_duration: Duration,
    total_duration: Duration,
    er_diag: ErChunkDiagnostics,
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
        er_diag,
    }
}

fn is_transient_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();

    // Common network / RPC transient markers
    if msg.contains("timeout")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::er::{
        BlockhashSource, ErBlockhash, ErBlockhashPlan, ErOutput, ErRouteInfo, ErSession,
        ErTelemetry, MockErClient,
    };
    use solana_program::instruction::{AccountMeta, Instruction};
    use std::sync::Arc;
    use tokio::runtime::Builder;
    use url::Url;

    #[test]
    fn er_mock_client_prepares_settlement_intents() {
        let mock = MockErClient::new();
        let route = ErRouteInfo {
            endpoint: Url::parse("http://localhost").unwrap(),
            ttl: Duration::from_secs(1),
            fetched_at: Instant::now(),
            cache_hit: false,
        };
        let session = ErSession {
            id: "mock-session".to_string(),
            route: route.clone(),
            endpoint: route.endpoint.clone(),
            accounts: vec![Pubkey::new_unique()],
            started_at: Instant::now(),
            route_duration: Duration::from_millis(5),
            blockhash_plan: None,
        };

        let settlement_ix = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![AccountMeta::new(Pubkey::new_unique(), false)],
            data: vec![1, 2, 3],
        };
        let settlement_accounts = vec![Pubkey::new_unique()];
        let output = ErOutput {
            settlement_instructions: vec![settlement_ix.clone()],
            settlement_accounts: settlement_accounts.clone(),
            plan_fingerprint: None,
            exec_duration: Duration::from_millis(2),
            blockhash_plan: Some(ErBlockhashPlan {
                route: route.clone(),
                blockhash: ErBlockhash {
                    hash: solana_sdk::hash::Hash::new_unique(),
                    last_valid_block_height: 42,
                    fetched_from: route.endpoint.clone(),
                    source: BlockhashSource::RouteEndpoint,
                    request_id: 1,
                },
                route_duration: Duration::from_millis(1),
                blockhash_duration: Duration::from_millis(1),
                blockhash_fetched_at: Instant::now(),
            }),
        };

        mock.push_begin(Ok(session));
        mock.push_execute(Ok(output));

        let telemetry = Arc::new(ErTelemetry::default());
        let ctx = ErExecutionCtx::new(Arc::new(mock), telemetry, true, false, 0, false);

        let payer = Pubkey::new_unique();
        let placeholder_intent = UserIntent::new(payer, settlement_ix.clone(), 0, None);
        let rt = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let prepared = rt
            .block_on(ctx.prepare_chunk(0, payer, &[placeholder_intent]))
            .expect("mock prepare");

        assert_eq!(prepared.intents.len(), 1);
        assert_eq!(prepared.intents[0].ix.program_id, settlement_ix.program_id);
        assert_eq!(prepared.meta.settlement_accounts, settlement_accounts);
        assert!(prepared.meta.blockhash_plan.is_some());
    }

    #[test]
    fn merge_small_chunks_merges_when_enabled() {
        let payer = Pubkey::new_unique();
        let make_plan = |index: usize, data: u8| -> ChunkPlan {
            let ix = Instruction {
                program_id: Pubkey::new_unique(),
                accounts: vec![AccountMeta::new(Pubkey::new_unique(), false)],
                data: vec![data],
            };
            let intent = UserIntent::new(payer, ix, 0, None);
            ChunkPlan {
                index,
                intents: vec![intent],
                estimates: ChunkEstimates {
                    est_cu: 60,
                    est_msg_bytes: 100,
                    alt_ro_count: 0,
                    alt_wr_count: 0,
                },
                merged_indices: Vec::new(),
            }
        };

        let baseline_plans = vec![make_plan(0, 1), make_plan(1, 2), make_plan(2, 3)];
        assert_eq!(baseline_plans.len(), 3, "baseline keeps individual chunks");

        let safety = SafetyMargins::default();
        let merged_plans = merge_small_chunk_plans(0, baseline_plans.clone(), 100, &safety);
        assert_eq!(
            merged_plans.len(),
            1,
            "merged plans collapse into a single chunk"
        );

        let merged = &merged_plans[0];
        assert_eq!(
            merged.merged_indices,
            vec![1, 2],
            "two additional chunks merged"
        );
        assert_eq!(merged.intents.len(), 3, "all intents combined");
        assert!(merged.estimates.est_cu >= 180);

        let telemetry = ErTelemetry::default();
        telemetry.record_small_chunk_merge(merged.merged_indices.len() as u64);
        assert_eq!(telemetry.summary().merged_small_chunks, 2);
    }
}
