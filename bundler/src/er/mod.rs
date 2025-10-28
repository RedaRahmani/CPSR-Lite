//! Ephemeral Rollup (ER) client abstractions and Magic Router-backed implementation.
//!
//! Phase 2 tasks require running the hot intent path inside an Ephemeral Rollup
//! session, receiving compact settlement instructions, and falling back to the
//! baseline direct-to-L1 path whenever the ER flow is unavailable.  This module
//! defines the common client interface and a Router-resolving implementation
//! that aligns with `docs.magicblock.gg` (router discovery + stateless routing).

use std::collections::{BTreeMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Context, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use chrono::Utc;
use cpsr_types::{
    hash::{dhash, Hash32, ZERO32},
    serde::InstructionSer,
    UserIntent,
};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client as HttpClient, StatusCode,
};
use serde::Deserialize;
use solana_program::{instruction::Instruction, pubkey::Pubkey};
use solana_sdk::hash::Hash;
use tokio::time::sleep;
use tracing::{debug, info, warn};
use url::Url;

pub mod bhfa;
pub mod router_client;
pub use router_client::HttpErClient;
use solana_sdk::{
    message::{Message, VersionedMessage},
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::VersionedTransaction,
};
use std::sync::Arc;

// Keep a bounded number of latency samples to avoid unbounded memory growth.
const TELEMETRY_WINDOW: usize = 1000;

/// Developer note:
/// curl --silent https://devnet-router.magicblock.app/getRoutes \
///   -H 'content-type: application/json' \
///   -d '{"jsonrpc":"2.0","id":1,"method":"getRoutes"}'
///
/// Node probe uses standard `getLatestBlockhash` at the base URL to validate JSON-RPC.
/// Optional CLI support (`--er-proxy-rpc`) can route simulate/send traffic through the
/// discovered Magic Router endpoint; Phase-1 execute fallbacks now emit a single info log.
///
/// Explicit privacy tier for route discovery.
#[derive(Clone, Copy, Debug)]
pub enum ErPrivacyMode {
    Public,
    Private,
}

impl ErPrivacyMode {
    fn as_str(&self) -> &'static str {
        match self {
            ErPrivacyMode::Public => "public",
            ErPrivacyMode::Private => "private",
        }
    }
}

/// Configuration knobs for the router-backed client.
#[derive(Clone, Debug)]
pub struct ErClientConfig {
    pub endpoint_override: Option<Url>,
    pub http_timeout: Duration,
    pub connect_timeout: Duration,
    pub session_ttl: Duration,
    pub retries: usize,
    pub privacy_mode: ErPrivacyMode,
    pub router_url: Url,
    pub router_api_key: Option<String>,
    pub route_cache_ttl: Duration,
    pub circuit_breaker_failures: u32,
    pub circuit_breaker_cooldown: Duration,
    pub payer: Arc<Keypair>,
    pub blockhash_cache_ttl: Duration,
    pub min_cu_threshold: u64,
    pub merge_small_intents: bool,
    pub require_router: bool,
    pub wiretap_verify_blockhash: bool,
    pub skip_preflight_on_router: bool,
    pub skip_preflight_on_override: bool,
    pub telemetry: Option<Arc<ErTelemetry>>,
    pub wiretap: Option<Arc<dyn ErWiretap>>,
}

#[derive(Clone, Debug)]
pub struct ErRouteInfo {
    pub endpoint: Url,
    pub ttl: Duration,
    pub fetched_at: Instant,
    pub cache_hit: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockhashSource {
    Router,
    RouteEndpoint,
}

impl BlockhashSource {
    fn as_str(&self) -> &'static str {
        match self {
            BlockhashSource::Router => "router",
            BlockhashSource::RouteEndpoint => "route_endpoint",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitPolicy {
    /// Submit via Magic Router without a local preflight. Chosen when the submit host is the
    /// Router itself (same-origin as router_url). Used after BHFA sourced from the Router.
    RouterNoPreflight,
    /// Submit to a user-provided override RPC with a local simulateTransaction preflight first.
    /// This is the conservative baseline when BHFA did not come from the Router.
    RpcSimulateThenSend,
    /// Submit to the override RPC first; when host parity indicates a blockhash mismatch
    /// (e.g., JSON-RPC error code -32002 Blockhash not found), retry via Magic Router with
    /// skipPreflight=true. This preserves latency while ensuring liveness when the override
    /// lags the Router.
    OverrideThenRouterParityRetry,
}

impl SubmitPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            SubmitPolicy::RouterNoPreflight => "router_no_preflight",
            SubmitPolicy::RpcSimulateThenSend => "rpc_simulate_then_send",
            SubmitPolicy::OverrideThenRouterParityRetry => "override_then_router_parity_retry",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ErBlockhash {
    pub hash: Hash,
    pub last_valid_block_height: u64,
    pub fetched_from: Url,
    pub source: BlockhashSource,
    pub request_id: u64,
}

#[derive(Clone, Debug)]
pub struct ErBlockhashPlan {
    pub route: ErRouteInfo,
    pub blockhash: ErBlockhash,
    pub route_duration: Duration,
    pub blockhash_duration: Duration,
    pub blockhash_fetched_at: Instant,
}

#[derive(Clone, Debug)]
pub struct ErHosts {
    pub bhfa: Url,
    pub submit: Url,
}

impl ErClientConfig {
    pub fn resolve_hosts(&self) -> ErHosts {
        if let Some(override_url) = &self.endpoint_override {
            ErHosts {
                bhfa: self.router_url.clone(),
                submit: override_url.clone(),
            }
        } else {
            ErHosts {
                bhfa: self.router_url.clone(),
                submit: self.router_url.clone(),
            }
        }
    }
}

/// Unique identifier assigned by MagicBlock for an ER session lifecycle.
pub type ErSessionId = String;

/// Handle describing an active ER session (route + blockhash context).
#[derive(Clone, Debug)]
pub struct ErSession {
    pub id: ErSessionId,
    pub route: ErRouteInfo,
    pub endpoint: Url,
    pub hosts: ErHosts,
    pub accounts: Vec<Pubkey>,
    pub started_at: Instant,
    pub route_duration: Duration,
    pub blockhash_plan: Option<ErBlockhashPlan>,
    pub data_plane: bool,
    pub fallback_reason: Option<String>,
}

/// Result of executing a chunk inside an ER session.
#[derive(Clone, Debug)]
pub struct ErOutput {
    pub settlement_instructions: Vec<Instruction>,
    pub settlement_accounts: Vec<Pubkey>,
    pub plan_fingerprint: Option<Hash32>,
    pub exec_duration: Duration,
    pub blockhash_plan: Option<ErBlockhashPlan>,
    pub signature: Option<String>,
    pub preflight_skipped: bool,
    pub submit_policy: SubmitPolicy,
    pub final_submit_host: Url,
    pub parity_retry: bool,
}

/// Aggregated telemetry for ER attempts.
#[derive(Default, Debug)]
pub struct ErTelemetry {
    sessions: AtomicU64,
    successes: AtomicU64,
    fallbacks: AtomicU64,
    routes_ok: AtomicU64,
    routes_err: AtomicU64,
    bhfa_ok: AtomicU64,
    bhfa_err: AtomicU64,
    router_simulate: AtomicU64,
    router_send: AtomicU64,
    route_ms: Mutex<VecDeque<u128>>,
    bhfa_ms: Mutex<VecDeque<u128>>,
    exec_ms: Mutex<VecDeque<u128>>,
    exec_ok: AtomicU64,
    exec_err: AtomicU64,
    router_cache_hits: AtomicU64,
    router_cache_misses: AtomicU64,
    router_error_kinds: Mutex<BTreeMap<String, u64>>,
    session_begin_attempts: AtomicU64,
    session_begin_ok: AtomicU64,
    session_begin_err: AtomicU64,
    last_router_error: Mutex<Option<RouterErrorSnapshot>>,
    blockhash_cache_hits: AtomicU64,
    blockhash_cache_misses: AtomicU64,
    small_chunk_skips: AtomicU64,
    merged_small_chunks: AtomicU64,
    dp_begin_ok: AtomicU64,
    dp_begin_err: AtomicU64,
    dp_execute_ok: AtomicU64,
    dp_execute_err: AtomicU64,
    dp_end_ok: AtomicU64,
    dp_end_err: AtomicU64,
    blockhash_plan_hits: AtomicU64,
    blockhash_plan_misses: AtomicU64,
    parity_retry_router: AtomicU64,
}

#[derive(Clone, Debug, Default)]
pub struct ErTelemetrySummary {
    pub sessions: u64,
    pub successes: u64,
    pub fallbacks: u64,
    pub routes_ok: u64,
    pub routes_err: u64,
    pub bhfa_ok: u64,
    pub bhfa_err: u64,
    pub exec_ok: u64,
    pub exec_err: u64,
    pub route_ms_p50: Option<f64>,
    pub route_ms_p95: Option<f64>,
    pub bhfa_ms_p50: Option<f64>,
    pub bhfa_ms_p95: Option<f64>,
    pub exec_ms_p50: Option<f64>,
    pub exec_ms_p95: Option<f64>,
    pub success_rate: f64,
    pub router_simulate: u64,
    pub router_send: u64,
    pub router_cache_hits: u64,
    pub router_cache_misses: u64,
    pub router_error_kinds: BTreeMap<String, u64>,
    pub session_begin_attempts: u64,
    pub session_begin_ok: u64,
    pub session_begin_err: u64,
    pub last_router_error_code: Option<i64>,
    pub last_router_error_message: Option<String>,
    pub blockhash_cache_hits: u64,
    pub blockhash_cache_misses: u64,
    pub small_chunk_skips: u64,
    pub merged_small_chunks: u64,
    pub dp_begin_ok: u64,
    pub dp_begin_err: u64,
    pub dp_execute_ok: u64,
    pub dp_execute_err: u64,
    pub dp_end_ok: u64,
    pub dp_end_err: u64,
    pub blockhash_plan_hits: u64,
    pub blockhash_plan_misses: u64,
    pub parity_retry_router: u64,
}

#[derive(Clone, Debug, Default)]
struct RouterErrorSnapshot {
    code: Option<i64>,
    message: String,
}

impl ErTelemetry {
    pub fn record_session_begin_attempt(&self) {
        self.session_begin_attempts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_session_begin_ok(&self) {
        self.session_begin_ok.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_session_begin_err(&self) {
        self.session_begin_err.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_last_router_error(&self, err: &anyhow::Error) {
        if let Ok(mut slot) = self.last_router_error.lock() {
            let mut snapshot = RouterErrorSnapshot::default();
            if let Some(rpc_err) = err
                .chain()
                .find_map(|cause| cause.downcast_ref::<RouterJsonRpcError>())
            {
                snapshot.code = Some(rpc_err.code);
                snapshot.message = rpc_err.message.clone();
            } else if let Some(router_err) = err
                .chain()
                .find_map(|cause| cause.downcast_ref::<RouterRequestError>())
            {
                snapshot.message = router_err.to_string();
            } else {
                snapshot.message = err.to_string();
            }
            *slot = Some(snapshot);
        }
    }

    pub fn record_route(&self, duration: Option<Duration>, success: bool) {
        if success {
            self.routes_ok.fetch_add(1, Ordering::Relaxed);
            if let Some(dur) = duration {
                if let Ok(mut samples) = self.route_ms.lock() {
                    samples.push_back(dur.as_millis());
                    while samples.len() > TELEMETRY_WINDOW {
                        samples.pop_front();
                    }
                }
            }
        } else {
            self.routes_err.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_blockhash(&self, _duration: Option<Duration>, success: bool) {
        self.sessions.fetch_add(1, Ordering::Relaxed);
        if success {
            self.successes.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_bhfa_success(&self, duration: Duration) {
        self.bhfa_ok.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut samples) = self.bhfa_ms.lock() {
            samples.push_back(duration.as_millis());
            while samples.len() > TELEMETRY_WINDOW {
                samples.pop_front();
            }
        }
    }

    pub fn record_bhfa_failure(&self) {
        self.bhfa_err.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_fallback(&self) {
        self.fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_execute(&self, duration: Option<Duration>, success: bool) {
        if success {
            self.exec_ok.fetch_add(1, Ordering::Relaxed);
            if let Some(dur) = duration {
                if let Ok(mut samples) = self.exec_ms.lock() {
                    samples.push_back(dur.as_millis());
                    while samples.len() > TELEMETRY_WINDOW {
                        samples.pop_front();
                    }
                }
            }
        } else {
            self.exec_err.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_route_cache(&self, hit: bool) {
        if hit {
            self.router_cache_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.router_cache_misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_router_error_kind(&self, kind: RouterErrorKind) {
        if let Ok(mut map) = self.router_error_kinds.lock() {
            let entry = map.entry(kind.as_str().to_string()).or_insert(0);
            *entry += 1;
        }
    }

    pub fn record_router_usage(&self, simulate_used: bool, send_used: bool) {
        if simulate_used {
            self.router_simulate.fetch_add(1, Ordering::Relaxed);
        }
        if send_used {
            self.router_send.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_dp_begin(&self, success: bool) {
        if success {
            self.dp_begin_ok.fetch_add(1, Ordering::Relaxed);
        } else {
            self.dp_begin_err.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_dp_execute(&self, success: bool) {
        if success {
            self.dp_execute_ok.fetch_add(1, Ordering::Relaxed);
        } else {
            self.dp_execute_err.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_dp_end(&self, success: bool) {
        if success {
            self.dp_end_ok.fetch_add(1, Ordering::Relaxed);
        } else {
            self.dp_end_err.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_blockhash_plan(&self, hit: bool) {
        if hit {
            self.blockhash_plan_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.blockhash_plan_misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_blockhash_cache(&self, hit: bool) {
        if hit {
            self.blockhash_cache_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.blockhash_cache_misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_parity_retry_router(&self) {
        self.parity_retry_router.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_small_chunk_skip(&self) {
        self.small_chunk_skips.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_small_chunk_merge(&self, merged_count: u64) {
        if merged_count > 0 {
            self.merged_small_chunks
                .fetch_add(merged_count, Ordering::Relaxed);
        }
    }

    pub fn summary(&self) -> ErTelemetrySummary {
        let sessions = self.sessions.load(Ordering::Relaxed);
        let successes = self.successes.load(Ordering::Relaxed);
        let fallbacks = self.fallbacks.load(Ordering::Relaxed);
        let routes_ok = self.routes_ok.load(Ordering::Relaxed);
        let routes_err = self.routes_err.load(Ordering::Relaxed);
        let bhfa_ok = self.bhfa_ok.load(Ordering::Relaxed);
        let bhfa_err = self.bhfa_err.load(Ordering::Relaxed);
        let router_simulate = self.router_simulate.load(Ordering::Relaxed);
        let router_send = self.router_send.load(Ordering::Relaxed);
        let exec_ok = self.exec_ok.load(Ordering::Relaxed);
        let exec_err = self.exec_err.load(Ordering::Relaxed);
        let router_cache_hits = self.router_cache_hits.load(Ordering::Relaxed);
        let router_cache_misses = self.router_cache_misses.load(Ordering::Relaxed);
        let blockhash_cache_hits = self.blockhash_cache_hits.load(Ordering::Relaxed);
        let blockhash_cache_misses = self.blockhash_cache_misses.load(Ordering::Relaxed);
        let small_chunk_skips = self.small_chunk_skips.load(Ordering::Relaxed);
        let merged_small_chunks = self.merged_small_chunks.load(Ordering::Relaxed);
        let session_begin_attempts = self.session_begin_attempts.load(Ordering::Relaxed);
        let session_begin_ok = self.session_begin_ok.load(Ordering::Relaxed);
        let session_begin_err = self.session_begin_err.load(Ordering::Relaxed);
        let dp_begin_ok = self.dp_begin_ok.load(Ordering::Relaxed);
        let dp_begin_err = self.dp_begin_err.load(Ordering::Relaxed);
        let dp_execute_ok = self.dp_execute_ok.load(Ordering::Relaxed);
        let dp_execute_err = self.dp_execute_err.load(Ordering::Relaxed);
        let dp_end_ok = self.dp_end_ok.load(Ordering::Relaxed);
        let dp_end_err = self.dp_end_err.load(Ordering::Relaxed);
        let blockhash_plan_hits = self.blockhash_plan_hits.load(Ordering::Relaxed);
        let blockhash_plan_misses = self.blockhash_plan_misses.load(Ordering::Relaxed);
        let parity_retry_router = self.parity_retry_router.load(Ordering::Relaxed);
        let last_router_error = self
            .last_router_error
            .lock()
            .map(|m| m.clone())
            .unwrap_or(None);

        let route_ms: Vec<u128> = self
            .route_ms
            .lock()
            .map(|v| v.iter().copied().collect())
            .unwrap_or_default();
        let bhfa_ms: Vec<u128> = self
            .bhfa_ms
            .lock()
            .map(|v| v.iter().copied().collect())
            .unwrap_or_default();
        let exec_ms: Vec<u128> = self
            .exec_ms
            .lock()
            .map(|v| v.iter().copied().collect())
            .unwrap_or_default();
        let router_error_kinds = self
            .router_error_kinds
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        let (last_error_code, last_error_message) = match last_router_error {
            Some(snapshot) => (snapshot.code, Some(snapshot.message)),
            None => (None, None),
        };

        ErTelemetrySummary {
            sessions,
            successes,
            fallbacks,
            routes_ok,
            routes_err,
            bhfa_ok,
            bhfa_err,
            exec_ok,
            exec_err,
            route_ms_p50: percentile(&route_ms, 0.50),
            route_ms_p95: percentile(&route_ms, 0.95),
            bhfa_ms_p50: percentile(&bhfa_ms, 0.50),
            bhfa_ms_p95: percentile(&bhfa_ms, 0.95),
            exec_ms_p50: percentile(&exec_ms, 0.50),
            exec_ms_p95: percentile(&exec_ms, 0.95),
            success_rate: if sessions > 0 {
                successes as f64 / sessions as f64
            } else {
                0.0
            },
            router_simulate,
            router_send,
            router_cache_hits,
            router_cache_misses,
            router_error_kinds,
            session_begin_attempts,
            session_begin_ok,
            session_begin_err,
            last_router_error_code: last_error_code,
            last_router_error_message: last_error_message,
            blockhash_cache_hits,
            blockhash_cache_misses,
            small_chunk_skips,
            merged_small_chunks,
            dp_begin_ok,
            dp_begin_err,
            dp_execute_ok,
            dp_execute_err,
            dp_end_ok,
            dp_end_err,
            blockhash_plan_hits,
            blockhash_plan_misses,
            parity_retry_router,
        }
    }
}

fn percentile(samples: &[u128], pct: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted: Vec<u128> = samples.to_vec();
    sorted.sort_unstable();
    let idx = ((sorted.len() as f64 - 1.0) * pct).ceil() as usize;
    let idx = idx.min(sorted.len().saturating_sub(1));
    Some(sorted[idx] as f64)
}

#[allow(dead_code)]
fn decode_instructions(value: Option<serde_json::Value>) -> Result<Vec<Instruction>> {
    if let Some(v) = value {
        let ser_vec: Vec<InstructionSer> =
            serde_json::from_value(v).context("instructions payload")?;
        Ok(ser_vec.into_iter().map(Instruction::from).collect())
    } else {
        Ok(Vec::new())
    }
}

#[allow(dead_code)]
fn decode_accounts(value: Option<serde_json::Value>) -> Result<Vec<Pubkey>> {
    if let Some(v) = value {
        let raw: Vec<String> = serde_json::from_value(v).context("accounts payload")?;
        let mut out = Vec::with_capacity(raw.len());
        for s in raw {
            let key = Pubkey::from_str(&s)
                .map_err(|e| anyhow!("invalid settlement account {}: {e}", s))?;
            out.push(key);
        }
        Ok(out)
    } else {
        Ok(Vec::new())
    }
}

#[allow(dead_code)]
fn parse_plan_fingerprint(value: Option<&serde_json::Value>) -> Result<Option<Hash32>> {
    match value {
        Some(v) if v.is_null() => Ok(None),
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| anyhow!("planFingerprint must be a string"))?;
            let fp = decode_hash32_from_str(s)?;
            Ok(Some(fp))
        }
        None => Ok(None),
    }
}

#[allow(dead_code)]
fn decode_hash32_from_str(input: &str) -> Result<Hash32> {
    if input.is_empty() {
        bail!("empty fingerprint string");
    }

    if let Ok(bytes) = bs58::decode(input).into_vec() {
        if bytes.len() == 32 {
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            return Ok(out);
        }
    }

    if let Ok(bytes) = BASE64_STANDARD.decode(input) {
        if bytes.len() == 32 {
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            return Ok(out);
        }
    }

    bail!("unsupported fingerprint encoding (expected base58/base64, 32 bytes)");
}

#[allow(dead_code)]
fn extract_blockhash_plan(
    value: Option<serde_json::Value>,
    route: &ErRouteInfo,
) -> Result<Option<ErBlockhashPlan>> {
    let Some(v) = value else {
        return Ok(None);
    };

    let payload: BlockhashRpcResult = serde_json::from_value(v).context("blockhash payload")?;
    let hash = Hash::from_str(&payload.blockhash).map_err(|e| anyhow!("invalid blockhash: {e}"))?;

    Ok(Some(ErBlockhashPlan {
        route: route.clone(),
        blockhash: ErBlockhash {
            hash,
            last_valid_block_height: payload.last_valid_block_height,
            fetched_from: route.endpoint.clone(),
            source: BlockhashSource::RouteEndpoint,
            request_id: ROUTER_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
        },
        route_duration: Duration::ZERO,
        blockhash_duration: Duration::ZERO,
        blockhash_fetched_at: Instant::now(),
    }))
}

pub fn classify_router_error(err: &anyhow::Error) -> Option<RouterErrorKind> {
    for cause in err.chain() {
        if let Some(router_err) = cause.downcast_ref::<RouterRequestError>() {
            return Some(router_err.kind());
        }
    }
    None
}

#[cfg(test)]
pub struct MockErClient {
    begin_queue: Mutex<std::collections::VecDeque<Result<ErSession>>>,
    execute_queue: Mutex<std::collections::VecDeque<Result<ErOutput>>>,
    pub end_calls: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
impl MockErClient {
    pub fn new() -> Self {
        Self {
            begin_queue: Mutex::new(std::collections::VecDeque::new()),
            execute_queue: Mutex::new(std::collections::VecDeque::new()),
            end_calls: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn push_begin(&self, result: Result<ErSession>) {
        self.begin_queue.lock().unwrap().push_back(result);
    }

    pub fn push_execute(&self, result: Result<ErOutput>) {
        self.execute_queue.lock().unwrap().push_back(result);
    }
}

#[cfg(test)]
#[async_trait]
impl ErClient for MockErClient {
    async fn begin_session(&self, _accounts: &[Pubkey]) -> Result<ErSession> {
        self.begin_queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(anyhow!("no mock begin result configured")))
    }

    async fn execute(&self, _session: &ErSession, _intents: &[UserIntent]) -> Result<ErOutput> {
        self.execute_queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(anyhow!("no mock execute result configured")))
    }

    async fn end_session(&self, _session: &ErSession) -> Result<()> {
        self.end_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// Compute a domain-separated, order-sensitive fingerprint for a list of intents.
pub fn compute_plan_fingerprint(intents: &[UserIntent]) -> Hash32 {
    if intents.is_empty() {
        return ZERO32;
    }

    // Domain separation via dhash to make the fingerprint distinct from other
    // intent-context hashes in CPSR-Lite.
    let hashes: Vec<Hash32> = intents.iter().map(|i| i.planner_fingerprint()).collect();
    let parts: Vec<&[u8]> = hashes.iter().map(|h| h.as_ref()).collect();
    dhash(b"CPSR:ER_PLAN_V1", &parts)
}

fn intents_to_instructions(intents: &[UserIntent]) -> anyhow::Result<Vec<Instruction>> {
    if intents.is_empty() {
        anyhow::bail!("no intents provided for ER execution");
    }

    // Reuse the canonical intent->instruction path (UserIntent::ix) used by the
    // baseline rollup pipeline builders.
    let mut out = Vec::with_capacity(intents.len());
    for (idx, intent) in intents.iter().enumerate() {
        let ix = intent.ix.clone();
        // A default program id with no accounts/data is a strong signal that the intent
        // payload never got populated. Guard early so downstream errors are actionable.
        if ix.program_id == Pubkey::default() && ix.accounts.is_empty() && ix.data.is_empty() {
            anyhow::bail!("intent at index {idx} is missing instruction payload");
        }
        out.push(ix);
    }
    Ok(out)
}

fn build_and_sign_transaction(
    instructions: &[Instruction],
    payer: &Keypair,
    recent_blockhash: Hash,
) -> anyhow::Result<VersionedTransaction> {
    if instructions.is_empty() {
        bail!("cannot build transaction without instructions");
    }

    let mut msg = Message::new(instructions, Some(&payer.pubkey()));
    msg.recent_blockhash = recent_blockhash;

    let vmsg = VersionedMessage::Legacy(msg);
    VersionedTransaction::try_new(vmsg, &[payer])
        .map_err(|e| anyhow::anyhow!("sign transaction: {e}"))
}

fn encode_transaction_base64(tx: &VersionedTransaction) -> anyhow::Result<String> {
    let bytes =
        bincode::serialize(tx).map_err(|e| anyhow::anyhow!("serialize transaction: {e}"))?;
    Ok(BASE64_STANDARD.encode(bytes))
}

fn is_blockhash_not_found(error: &serde_json::Value) -> bool {
    let check_message = |msg: &str| msg.to_ascii_lowercase().contains("blockhash not found");

    // Prefer structured JSON-RPC code checks when available.
    if let Some(code) = error.get("code").and_then(|c| c.as_i64()) {
        if code == -32002 {
            // Magic Router and some RPCs use -32002 for blockhash parity issues.
            return true;
        }
    }

    if let Some(msg) = error.get("message").and_then(|m| m.as_str()) {
        if check_message(msg) {
            return true;
        }
    }

    if let Some(data_msg) = error
        .get("data")
        .and_then(|data| data.get("message"))
        .and_then(|m| m.as_str())
    {
        if check_message(data_msg) {
            return true;
        }
    }

    false
}

fn is_blockhash_domain_error(error: &serde_json::Value) -> bool {
    if is_blockhash_not_found(error) {
        return true;
    }

    let mentions_blockhash = |msg: &str| {
        let lower = msg.to_ascii_lowercase();
        lower.contains("blockhash")
    };

    if let Some(msg) = error.get("message").and_then(|m| m.as_str()) {
        if mentions_blockhash(msg) {
            return true;
        }
    }

    if let Some(data_msg) = error
        .get("data")
        .and_then(|data| data.get("message"))
        .and_then(|m| m.as_str())
    {
        if mentions_blockhash(data_msg) {
            return true;
        }
    }

    false
}

fn ensure_simulation_success(sim: &serde_json::Value) -> Result<()> {
    if let Some(err) = sim.get("error") {
        bail!("simulateTransaction RPC error: {err}");
    }
    if sim
        .get("result")
        .and_then(|r| r.get("err"))
        .filter(|e| !e.is_null())
        .is_some()
    {
        bail!("simulateTransaction program error: {sim}");
    }
    Ok(())
}

/// Primary trait for Ephemeral Rollup clients.
#[async_trait]
pub trait ErClient: Send + Sync {
    async fn begin_session(&self, accounts: &[Pubkey]) -> Result<ErSession>;
    async fn execute(&self, session: &ErSession, intents: &[UserIntent]) -> Result<ErOutput>;
    async fn end_session(&self, session: &ErSession) -> Result<()>;
}

/// Sink for emitting ER wiretap payloads.
pub trait ErWiretap: Send + Sync + std::fmt::Debug {
    fn write_json(&self, filename: &str, value: &serde_json::Value);
    fn write_string(&self, filename: &str, body: &str);
}

/// Router-resolving ER client (HTTP).
pub struct ErHttpClientInner {
    cfg: ErClientConfig,
    http: HttpClient,
    route_cache: Mutex<Option<CachedRoute>>,
    // Simple circuit breaker state for router discovery failures.
    circuit: Mutex<CircuitState>,
    data_plane_api_key: Option<HeaderValue>,
    #[allow(dead_code)]
    blockhash_cache: Mutex<Option<BlockhashCacheEntry>>,
    telemetry: Option<Arc<ErTelemetry>>,
    wiretap: Option<Arc<dyn ErWiretap>>,
    router_bhfa_degraded: AtomicBool,
    #[allow(dead_code)]
    data_plane_announced: AtomicBool,
}

#[allow(dead_code)]
static ROUTER_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_router_request_id() -> u64 {
    ROUTER_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

fn rpc_context(method: &str, url: &Url, request_id: u64) -> String {
    let host = url.host_str().unwrap_or("<unknown>");
    format!("{method} via host={host}, request_id={request_id}")
}

struct CachedRoute {
    endpoint: Url,
    fetched_at: Instant,
    ttl: Duration,
}

#[allow(dead_code)]
struct BlockhashCacheEntry {
    hash: Hash,
    fetched_at: Instant,
}

#[derive(Default)]
struct CircuitState {
    failures: u32,
    tripped_until: Option<Instant>,
}

impl CircuitState {
    fn record_failure(&mut self, threshold: u32, cooldown: Duration) -> Option<Instant> {
        if threshold == 0 {
            return None;
        }
        self.failures = self.failures.saturating_add(1);
        if self.failures >= threshold {
            let until = Instant::now() + cooldown;
            self.tripped_until = Some(until);
            Some(until)
        } else {
            None
        }
    }

    fn reset(&mut self) -> bool {
        let was_active = self.failures > 0 || self.tripped_until.is_some();
        self.failures = 0;
        self.tripped_until = None;
        was_active
    }

    fn is_tripped(&mut self) -> Option<Instant> {
        if let Some(until) = self.tripped_until {
            if Instant::now() < until {
                return Some(until);
            }
            self.tripped_until = None;
            self.failures = 0;
        }
        None
    }
}

struct JsonResponse {
    status: StatusCode,
    value: serde_json::Value,
}

impl ErHttpClientInner {
    pub fn new(cfg: ErClientConfig) -> Result<Self> {
        let timeout = cfg.http_timeout.max(Duration::from_millis(100));
        let connect_timeout = cfg.connect_timeout.max(Duration::from_millis(100));
        let http = HttpClient::builder()
            .timeout(timeout)
            .connect_timeout(connect_timeout)
            .build()
            .context("building ER HTTP client")?;
        Self::with_http_client(cfg, http)
    }

    pub fn with_http_client(cfg: ErClientConfig, http: HttpClient) -> Result<Self> {
        let data_plane_api_key = match cfg.privacy_mode {
            ErPrivacyMode::Private => cfg
                .router_api_key
                .as_ref()
                .map(|value| HeaderValue::from_str(value))
                .transpose()
                .context("router api key header")?,
            ErPrivacyMode::Public => None,
        };

        let telemetry = cfg.telemetry.clone();
        let wiretap = cfg.wiretap.clone();

        Ok(Self {
            cfg,
            http,
            route_cache: Mutex::new(None),
            circuit: Mutex::new(CircuitState::default()),
            data_plane_api_key,
            blockhash_cache: Mutex::new(None),
            telemetry,
            wiretap,
            router_bhfa_degraded: AtomicBool::new(false),
            data_plane_announced: AtomicBool::new(false),
        })
    }

    fn getroutes_url(router_base: &Url) -> Url {
        // The Magic Router supports method-as-path (…/getRoutes) as shown in docs.
        // Fall back to the base if join fails (very unlikely with a valid base URL).
        router_base
            .join("getRoutes")
            .unwrap_or_else(|_| router_base.clone())
    }

    async fn resolve_route(&self) -> Result<ErRouteInfo> {
        if let Some(override_url) = &self.cfg.endpoint_override {
            let ttl = self.cfg.route_cache_ttl;
            return Ok(ErRouteInfo {
                endpoint: override_url.clone(),
                ttl,
                fetched_at: Instant::now(),
                cache_hit: false,
            });
        }

        if let Some(route) = self.cached_route() {
            debug!(
                target: "er",
                route = route.endpoint.as_str(),
                "using cached ER route"
            );
            return Ok(route);
        }

        let fresh = self.fetch_route_from_router().await?;
        let info = ErRouteInfo {
            endpoint: fresh.endpoint.clone(),
            ttl: fresh.ttl,
            fetched_at: fresh.fetched_at,
            cache_hit: false,
        };
        if let Ok(mut cache_guard) = self.route_cache.lock() {
            *cache_guard = Some(fresh);
        } else {
            warn!(target: "er", "route cache mutex poisoned while inserting fresh route");
        }
        Ok(info)
    }

    fn cached_route(&self) -> Option<ErRouteInfo> {
        let mut cache_guard = match self.route_cache.lock() {
            Ok(g) => g,
            Err(_) => {
                warn!(target: "er", "route cache mutex poisoned; ignoring cached route");
                return None;
            }
        };
        match cache_guard.as_ref() {
            Some(entry) if entry.fetched_at + entry.ttl > Instant::now() => Some(ErRouteInfo {
                endpoint: entry.endpoint.clone(),
                ttl: entry.ttl,
                fetched_at: entry.fetched_at,
                cache_hit: true,
            }),
            Some(entry) => {
                let expired = entry.endpoint.clone();
                *cache_guard = None;
                debug!(
                    target: "er",
                    route = expired.as_str(),
                    "cached ER route expired"
                );
                None
            }
            None => None,
        }
    }

    fn should_apply_circuit(&self, url: &Url) -> bool {
        Self::same_origin(url, &self.cfg.router_url)
    }

    fn api_key_header_for_host(&self, host: &Url) -> Result<Option<HeaderValue>> {
        match self.cfg.privacy_mode {
            ErPrivacyMode::Private
                if Self::same_origin(host, &self.cfg.router_url)
                    && self.cfg.router_api_key.is_some() =>
            {
                self.cfg
                    .router_api_key
                    .as_ref()
                    .map(|key| HeaderValue::from_str(key).context("router api key header"))
                    .transpose()
            }
            _ => Ok(None),
        }
    }

    fn same_origin(a: &Url, b: &Url) -> bool {
        a.scheme() == b.scheme()
            && a.host_str() == b.host_str()
            && a.port_or_known_default() == b.port_or_known_default()
    }

    fn decide_submit_policy(&self, hosts: &ErHosts, bhfa_from_router: bool) -> SubmitPolicy {
        if Self::same_origin(&hosts.submit, &self.cfg.router_url) {
            return SubmitPolicy::RouterNoPreflight;
        }

        if bhfa_from_router {
            if let Some(override_url) = &self.cfg.endpoint_override {
                if Self::same_origin(&hosts.submit, override_url) {
                    return SubmitPolicy::OverrideThenRouterParityRetry;
                }
            }
        }

        SubmitPolicy::RpcSimulateThenSend
    }

    async fn post_json(
        &self,
        url: &Url,
        body: &serde_json::Value,
        api_key_header: Option<&HeaderValue>,
    ) -> Result<JsonResponse> {
        let apply_circuit = self.should_apply_circuit(url) && self.cfg.circuit_breaker_failures > 0;
        // Whether we should attempt post-send circuit updates
        let mut allow_circuit_updates = false;

        if apply_circuit {
            match self.circuit.lock() {
                Ok(mut circ) => {
                    if let Some(until) = circ.is_tripped() {
                        let remaining = until.saturating_duration_since(Instant::now());
                        bail!("router circuit breaker is tripped; retry after {:?}", remaining);
                    }
                    allow_circuit_updates = true;
                }
                Err(_) => {
                    warn!(target: "er", "circuit mutex poisoned; bypassing breaker pre-check");
                    allow_circuit_updates = false;
                }
            }
        }

        // Perform network send (no circuit guard held across await)
        let result = self.send_with_retries(url, body, api_key_header).await;

        // Post-send circuit accounting
        if apply_circuit && allow_circuit_updates {
            if let Ok(mut circ) = self.circuit.lock() {
                match &result {
                    Ok(_) => {
                        if circ.reset() {
                            debug!(target: "er", "router circuit breaker reset");
                        }
                    }
                    Err(_) => {
                        if let Some(until) = circ.record_failure(
                            self.cfg.circuit_breaker_failures,
                            self.cfg.circuit_breaker_cooldown,
                        ) {
                            warn!(
                                target: "er",
                                retry_after_ms = self.cfg.circuit_breaker_cooldown.as_millis(),
                                failures = self.cfg.circuit_breaker_failures,
                                "router circuit breaker opened; suppressing requests until {:?}",
                                until
                            );
                        }
                    }
                }
            } else {
                warn!(target: "er", "circuit mutex poisoned; skipping circuit update");
            }
        }

        result.map_err(|e| anyhow!(e))
    }

    async fn fetch_route_from_router(&self) -> Result<CachedRoute> {
        // Only pass x-api-key in Private mode (per docs and your checklist).
        let api_key_header = match self.cfg.privacy_mode {
            ErPrivacyMode::Private => self
                .cfg
                .router_api_key
                .as_ref()
                .map(|key| HeaderValue::from_str(key).context("router api key header"))
                .transpose()?,
            ErPrivacyMode::Public => None,
        };

        let discovery_url = Self::getroutes_url(&self.cfg.router_url);
        let request_id = next_router_request_id();

        // JSON-RPC envelope; privacy handled server-side and/or by x-api-key.
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":request_id,"method":"getRoutes"
        });

        let response = self
            .post_json(&discovery_url, &body, api_key_header.as_ref())
            .await
            .with_context(|| rpc_context("getRoutes", &discovery_url, request_id))?;

        if !response.status.is_success() {
            bail!("router discovery failed: {}", response.status);
        }

        let payload = response.value;

        if let Some(err) = payload.get("error") {
            if let Ok(rpc_err) = serde_json::from_value::<RouterRpcError>(err.clone()) {
                return Err(anyhow!(RouterJsonRpcError::new(
                    rpc_err.code,
                    rpc_err.message
                )));
            } else {
                bail!("router returned error payload: {}", err);
            }
        }

        let routes_value = payload
            .get("result")
            .cloned()
            .or_else(|| payload.get("routes").cloned())
            .ok_or_else(|| anyhow!("router response missing routes"))?;

        let mut routes: Vec<RouterRpcRoute> =
            serde_json::from_value(routes_value).context("decode router routes")?;
        if routes.is_empty() {
            bail!("router returned zero ER routes");
        }

        routes.retain(|route| route.matches_privacy(self.cfg.privacy_mode));
        if routes.is_empty() {
            bail!(
                "router returned no ER routes for privacy mode {}",
                self.cfg.privacy_mode.as_str()
            );
        }

        routes.sort_by(|a, b| a.fqdn.cmp(&b.fqdn));
        let chosen = routes
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("router returned empty route list after filtering"))?;

        let endpoint = Url::parse(&chosen.fqdn).context("route fqdn url")?;
        match endpoint.scheme() {
            "http" | "https" => {}
            other => bail!("unsupported ER route scheme: {}", other),
        }

        let ttl = chosen
            .ttl_duration()
            .map(|router_ttl| router_ttl.min(self.cfg.route_cache_ttl))
            .unwrap_or(self.cfg.route_cache_ttl);

        info!(
            target: "er",
            route = endpoint.as_str(),
            route_ttl_ms = ttl.as_millis(),
            privacy = self.cfg.privacy_mode.as_str(),
            "Magic Router discovery succeeded"
        );

        Ok(CachedRoute {
            endpoint,
            fetched_at: Instant::now(),
            ttl,
        })
    }

    async fn send_with_retries(
        &self,
        url: &Url,
        body: &serde_json::Value,
        api_key_header: Option<&HeaderValue>,
    ) -> Result<JsonResponse, RouterRequestError> {
        let total_attempts = self.cfg.retries.saturating_add(1);
        let mut last_err: Option<RouterRequestError> = None;

        for attempt_idx in 0..total_attempts {
            let attempt_num = attempt_idx + 1;
            let mut req = self
                .http
                .post(url.clone())
                .header(reqwest::header::ACCEPT, "application/json")
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(body);

            if let Some(hv) = api_key_header {
                req = req.header("x-api-key", hv.clone());
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let headers = resp.headers().clone();
                    let bytes = match resp.bytes().await {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            let kind = classify_reqwest_error(&e);
                            let msg = format!("failed to read response body: {e}");
                            last_err = Some(RouterRequestError::new(kind, msg));
                            break;
                        }
                    };
                    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
                        Ok(v) => v,
                        Err(err) => {
                            let msg = format!("invalid JSON response: {err}");
                            last_err = Some(RouterRequestError::new(RouterErrorKind::Other, msg));
                            break;
                        }
                    };

                    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                        let backoff = retry_backoff(attempt_idx, &headers);
                        let retrying = attempt_num < total_attempts;
                        warn!(
                            target: "er",
                            attempt = attempt_num,
                            attempts = total_attempts,
                            error_class = "http_server",
                            status = status.as_u16(),
                            retrying,
                            backoff_ms = backoff.as_millis(),
                            "router request returned retryable status"
                        );

                        last_err = Some(RouterRequestError::new(
                            RouterErrorKind::Request,
                            format!("server error {status}"),
                        ));

                        if retrying {
                            sleep(backoff).await;
                            continue;
                        } else {
                            break;
                        }
                    }

                    if let Some(code) = value
                        .get("error")
                        .and_then(|err| err.get("code"))
                        .and_then(|code| code.as_i64())
                    {
                        let meaning = json_rpc_code_meaning(code);
                        let retrying = is_retryable_jsonrpc(code) && attempt_num < total_attempts;
                        warn!(
                            target: "er",
                            attempt = attempt_num,
                            attempts = total_attempts,
                            error_class = "json_rpc",
                            code,
                            meaning = meaning,
                            retrying,
                            "router JSON-RPC error"
                        );

                        if retrying {
                            last_err = Some(RouterRequestError::new(
                                RouterErrorKind::Request,
                                format!("json-rpc error {code} ({meaning})"),
                            ));
                            sleep(compute_router_backoff(attempt_idx)).await;
                            continue;
                        }

                        return Ok(JsonResponse { status, value });
                    }

                    if attempt_num > 1 {
                        debug!(
                            target: "er",
                            attempt = attempt_num,
                            attempts = total_attempts,
                            status = status.as_u16(),
                            "router request succeeded after retries"
                        );
                    }

                    return Ok(JsonResponse { status, value });
                }
                Err(e) => {
                    let kind = classify_reqwest_error(&e);
                    let chain = error_chain_strings(&e);
                    let backoff = if attempt_num < total_attempts {
                        Some(compute_router_backoff(attempt_idx))
                    } else {
                        None
                    };

                    warn!(
                        target: "er",
                        attempt = attempt_num,
                        attempts = total_attempts,
                        error_class = kind.as_str(),
                        retrying = backoff.is_some(),
                        backoff_ms = backoff.map(|d| d.as_millis()).unwrap_or(0),
                        "router request failed: {e}"
                    );

                    last_err = Some(RouterRequestError::new(
                        kind,
                        format!("reqwest error: {e}; chain={chain:?}"),
                    ));

                    if let Some(delay) = backoff {
                        sleep(delay).await;
                    } else {
                        break;
                    }
                }
            }
        }

        let err = last_err.unwrap_or_else(|| {
            RouterRequestError::new(
                RouterErrorKind::Other,
                "operation failed without error detail",
            )
        });

        warn!(
            target: "er",
            attempts = total_attempts,
            "router request exhausted retries: {}",
            err
        );

        Err(err)
    }
    /// Header for data-plane calls (simulate/send) when in Private mode.
    #[inline]
    fn data_plane_api_key_header(&self) -> Option<&HeaderValue> {
        self.data_plane_api_key.as_ref()
    }

    fn wiretap(&self) -> Option<Arc<dyn ErWiretap>> {
        self.wiretap.clone()
    }

    fn wiretap_write_json(&self, filename: &str, value: &serde_json::Value) {
        if let Some(tap) = self.wiretap() {
            tap.write_json(filename, value);
        }
    }

    fn wiretap_write_string(&self, filename: &str, body: &str) {
        if let Some(tap) = self.wiretap() {
            tap.write_string(filename, body);
        }
    }

    fn wiretap_dp_timestamp() -> String {
        Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string()
    }

    fn wiretap_dp_filename(method: &str, ts: &str, suffix: &str) -> String {
        format!("dp-{method}-{ts}_{suffix}")
    }

    fn wiretap_guard(&self, label: &str, payload: serde_json::Value) {
        if let Some(tap) = self.wiretap() {
            let filename = format!(
                "guard-{}-{}.json",
                label,
                Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
            );
            tap.write_json(&filename, &payload);
        }
    }

    fn wiretap_bhfa_guard(
        &self,
        tx: &VersionedTransaction,
        plan: Option<&ErBlockhashPlan>,
    ) -> Result<()> {
        let tx_blockhash = match &tx.message {
            VersionedMessage::Legacy(msg) => msg.recent_blockhash,
            VersionedMessage::V0(msg) => msg.recent_blockhash,
        };

        let expected = plan.map(|p| p.blockhash.hash);
        let status = match expected {
            Some(expected_hash) if expected_hash == tx_blockhash => "match",
            Some(_) => "mismatch",
            None => "absent",
        };

        let signature_ready = !tx.signatures.is_empty()
            && tx.signatures.iter().any(|sig| *sig != Signature::default());

        let payload = serde_json::json!({
            "tx_blockhash": tx_blockhash.to_string(),
            "expected_blockhash": expected.map(|h| h.to_string()),
            "status": status,
            "signature_ready": signature_ready,
            "signature_count": tx.signatures.len(),
        });
        self.wiretap_guard("bhfa-blockhash", payload);

        let expected_str = expected.map(|h| h.to_string());
        if self.cfg.wiretap_verify_blockhash {
            ensure!(
                matches!(status, "match"),
                "BHFA wiretap guard mismatch: tx_blockhash={}, expected={:?}",
                tx_blockhash,
                expected_str
            );
            ensure!(
                signature_ready,
                "BHFA wiretap guard detected missing or default signatures"
            );
        } else {
            match status {
                "match" => {
                    debug!(
                        target: "er",
                        tx_blockhash = %tx_blockhash,
                        "BHFA wiretap guard confirmed transaction blockhash"
                    );
                }
                "mismatch" => {
                    warn!(
                        target: "er",
                        tx_blockhash = %tx_blockhash,
                        expected = expected_str.as_deref().unwrap_or("<none>"),
                        "BHFA wiretap guard detected blockhash mismatch"
                    );
                }
                "absent" => {
                    warn!(
                        target: "er",
                        tx_blockhash = %tx_blockhash,
                        "BHFA wiretap guard enabled but blockhash plan absent"
                    );
                }
                _ => {}
            }

            if !signature_ready {
                warn!(
                    target: "er",
                    "BHFA wiretap guard detected missing or default signatures"
                );
            }
        }

        Ok(())
    }

    #[allow(dead_code)]
    fn announce_data_plane(&self, endpoint: &Url) {
        if !self.data_plane_announced.swap(true, Ordering::Relaxed) {
            info!(
                target: "er.dp",
                endpoint = endpoint.as_str(),
                "data-plane mode ACTIVE"
            );
        }
    }

    fn convert_blockhash_plan(
        &self,
        route: &ErRouteInfo,
        plan: &DataPlaneBlockhashPlan,
        route_duration: Duration,
        blockhash_duration: Duration,
        source: BlockhashSource,
        fetched_from: Option<&Url>,
    ) -> Result<ErBlockhashPlan> {
        let hash = Hash::from_str(&plan.blockhash)
            .map_err(|e| anyhow!("invalid blockhash from data-plane: {e}"))?;
        Ok(ErBlockhashPlan {
            route: route.clone(),
            blockhash: ErBlockhash {
                hash,
                last_valid_block_height: plan.last_valid_slot,
                fetched_from: fetched_from
                    .cloned()
                    .unwrap_or_else(|| route.endpoint.clone()),
                source,
                request_id: next_router_request_id(),
            },
            route_duration,
            blockhash_duration,
            blockhash_fetched_at: Instant::now(),
        })
    }

    async fn fetch_blockhash_for_accounts_at(
        &self,
        host: &Url,
        accounts: &[Pubkey],
        route: &ErRouteInfo,
        route_duration: Duration,
    ) -> Result<ErBlockhashPlan> {
        let bhfa_start = Instant::now();
        let request_id = next_router_request_id();
        let ctx = rpc_context("getBlockhashForAccounts", host, request_id);

        let api_key_header = self.api_key_header_for_host(host)?;

        let account_strings: Vec<String> = accounts.iter().map(|pk| pk.to_string()).collect();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "getBlockhashForAccounts",
            "params": {
                "accounts": account_strings,
            },
        });

        let response = match self.post_json(host, &body, api_key_header.as_ref()).await {
            Ok(resp) => resp,
            Err(err) => {
                if let Some(tel) = &self.telemetry {
                    tel.record_bhfa_failure();
                }
                return Err(err).with_context(|| ctx.clone());
            }
        };

        let parse_result: Result<(Hash, u64)> = (|| {
            let payload = response.value;
            let result = payload
                .get("result")
                .ok_or_else(|| anyhow!("BHFA: missing result field"))?;

            let blockhash_str = result
                .get("blockhash")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("BHFA: missing blockhash"))?;

            let last_valid = result
                .get("lastValidBlockHeight")
                .or_else(|| result.get("lastValidSlot"))
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow!("BHFA: missing lastValidBlockHeight/lastValidSlot"))?;

            let hash = Hash::from_str(blockhash_str).context("BHFA: invalid blockhash format")?;
            Ok((hash, last_valid))
        })();

        let (hash, last_valid_slot) = match parse_result {
            Ok(res) => res,
            Err(err) => {
                if let Some(tel) = &self.telemetry {
                    tel.record_bhfa_failure();
                }
                return Err(err).with_context(|| ctx.clone());
            }
        };

        let bhfa_duration = bhfa_start.elapsed();
        if let Some(tel) = &self.telemetry {
            tel.record_bhfa_success(bhfa_duration);
        }

        let plan = DataPlaneBlockhashPlan {
            blockhash: hash.to_string(),
            last_valid_slot,
        };

        let source = if Self::same_origin(host, &self.cfg.router_url) {
            BlockhashSource::Router
        } else {
            BlockhashSource::RouteEndpoint
        };

        self.convert_blockhash_plan(
            route,
            &plan,
            route_duration,
            bhfa_duration,
            source,
            Some(host),
        )
    }

    async fn begin_session_router(&self, accounts: &[Pubkey]) -> Result<ErSession> {
        let was_degraded = self.router_bhfa_degraded.swap(false, Ordering::Relaxed);
        if was_degraded {
            debug!(target: "er", "router BHFA probe reset for new session");
        }

        let route_start = Instant::now();
        let route = self.resolve_route().await?;
        let route_duration = route_start.elapsed();
        let hosts = self.cfg.resolve_hosts();

        info!(
            target: "er.hosts",
            bhfa_host = hosts.bhfa.as_str(),
            submit_host = hosts.submit.as_str(),
            override_present = self.cfg.endpoint_override.is_some(),
            "ER host resolution established"
        );

        info!(
            target: "er",
            route = route.endpoint.as_str(),
            route_ms = route_duration.as_millis(),
            accounts = accounts.len(),
            "ER session context established (router discovery only)"
        );

        Ok(ErSession {
            id: "router-session".to_string(),
            route: route.clone(),
            endpoint: hosts.submit.clone(),
            hosts,
            accounts: accounts.to_vec(),
            started_at: Instant::now(),
            route_duration,
            blockhash_plan: None,
            data_plane: false,
            fallback_reason: None,
        })
    }

    async fn execute_router(
        &self,
        session: &ErSession,
        intents: &[UserIntent],
        telemetry: Option<&Arc<ErTelemetry>>,
    ) -> Result<ErOutput> {
        let instructions = intents_to_instructions(intents)
            .context("failed to convert intents into instructions")?;

        let hosts = &session.hosts;
        let bhfa_host = &hosts.bhfa;
        let submit_host = &hosts.submit;

        info!(
            target: "er",
            bhfa_host = bhfa_host.as_str(),
            submit_host = submit_host.as_str(),
            "ER execution host mapping"
        );

        let mut blockhash_plan_opt: Option<ErBlockhashPlan> = None;
        let mut router_err: Option<anyhow::Error> = None;
        let mut override_err: Option<anyhow::Error> = None;

        let override_host = self.cfg.endpoint_override.as_ref();
        let router_host = &self.cfg.router_url;
        let attempt_router = self.cfg.require_router
            || override_host.is_none()
            || !self.router_bhfa_degraded.load(Ordering::Relaxed);

        if attempt_router {
            match self
                .fetch_blockhash_for_accounts_at(
                    router_host,
                    &session.accounts,
                    &session.route,
                    session.route_duration,
                )
                .await
            {
                Ok(plan) => {
                    self.router_bhfa_degraded.store(false, Ordering::Relaxed);
                    blockhash_plan_opt = Some(plan);
                }
                Err(err) => {
                    if let Some(override_host) = override_host {
                        if self.cfg.require_router {
                            warn!(
                                target: "er",
                                bhfa_host = router_host.as_str(),
                                override_host = override_host.as_str(),
                                "BHFA via router failed under require_router: {:#}",
                                err
                            );
                        } else {
                            let first = !self.router_bhfa_degraded.swap(true, Ordering::Relaxed);
                            if first {
                                warn!(
                                    target: "er",
                                    bhfa_host = router_host.as_str(),
                                    fallback_host = override_host.as_str(),
                                    "BHFA via router failed; enabling override fallback: {:#}",
                                    err
                                );
                            } else {
                                debug!(
                                    target: "er",
                                    bhfa_host = router_host.as_str(),
                                    fallback_host = override_host.as_str(),
                                    "BHFA via router still degraded: {:#}",
                                    err
                                );
                            }
                        }
                    } else {
                        warn!(
                            target: "er",
                            bhfa_host = router_host.as_str(),
                            "BHFA via router failed: {:#}",
                            err
                        );
                    }
                    router_err = Some(err);
                }
            }
        } else if let Some(override_host) = override_host {
            debug!(
                target: "er",
                bhfa_host = router_host.as_str(),
                fallback_host = override_host.as_str(),
                "Skipping router BHFA probe; degraded flag active"
            );
        }

        if blockhash_plan_opt.is_none() && !self.cfg.require_router {
            if let Some(fallback_host) = override_host {
                match self
                    .fetch_blockhash_for_accounts_at(
                        fallback_host,
                        &session.accounts,
                        &session.route,
                        session.route_duration,
                    )
                    .await
                {
                    Ok(plan) => {
                        blockhash_plan_opt = Some(plan);
                    }
                    Err(err) => {
                        warn!(
                            target: "er",
                            bhfa_host = fallback_host.as_str(),
                            "BHFA via override host failed: {:#}",
                            err
                        );
                        override_err = Some(err);
                    }
                }
            }
        }

        if blockhash_plan_opt.is_none() && self.cfg.require_router {
            if let Some(err) = router_err {
                return Err(err).context("BHFA required but failed");
            }
            return Err(anyhow!("BHFA required but unavailable"));
        }

        if let Some(plan) = blockhash_plan_opt.as_ref() {
            info!(
                target: "er.bhfa",
                source = plan.blockhash.source.as_str(),
                fetched_from = plan.blockhash.fetched_from.as_str(),
                last_valid_block_height = plan.blockhash.last_valid_block_height,
                "BHFA plan acquired"
            );
        }

        if blockhash_plan_opt.is_none() {
            let mut detail_segments: Vec<String> = Vec::new();
            if let Some(err) = router_err.as_ref() {
                detail_segments.push(format!("router: {err:#}"));
            }
            if let Some(err) = override_err.as_ref() {
                detail_segments.push(format!("override: {err:#}"));
            }
            let detail = if detail_segments.is_empty() {
                "no BHFA attempt succeeded".to_string()
            } else {
                detail_segments.join(" | ")
            };
            warn!(
                target: "er",
                bhfa_host = bhfa_host.as_str(),
                submit_host = submit_host.as_str(),
                fallback_reason = "bhfa_error",
                require_router = self.cfg.require_router,
                "BHFA unavailable; {detail}"
            );
            if self.cfg.require_router {
                return Err(anyhow!("BHFA required but failed: {detail}"));
            }
            return Err(anyhow!("BHFA unavailable: {detail}"));
        }

        let bhfa_plan_ref = blockhash_plan_opt
            .as_ref()
            .expect("blockhash plan must exist after guard");
        let bhfa_source = bhfa_plan_ref.blockhash.source;
        let bhfa_from_router = matches!(bhfa_source, BlockhashSource::Router);
        let policy = self.decide_submit_policy(hosts, bhfa_from_router);

        let recent_blockhash = bhfa_plan_ref.blockhash.hash;

        let final_submit_host = submit_host.clone();
        let submit_reason = match policy {
            SubmitPolicy::RouterNoPreflight => "bhfa_source_router",
            SubmitPolicy::OverrideThenRouterParityRetry => "router_retry_on_parity",
            SubmitPolicy::RpcSimulateThenSend => "user_override_or_region_bhfa",
        };
        let run_simulation = match policy {
            SubmitPolicy::RouterNoPreflight => false,
            SubmitPolicy::RpcSimulateThenSend => true,
            SubmitPolicy::OverrideThenRouterParityRetry => !self.cfg.skip_preflight_on_override,
        };
        let mut preflight_skipped = !run_simulation;

        info!(
            target: "er.submit",
            host = final_submit_host.as_str(),
            reason = submit_reason,
            submit_policy = policy.as_str(),
            preflight_skipped,
            "ER submit host finalized"
        );

        let tx =
            build_and_sign_transaction(&instructions, self.cfg.payer.as_ref(), recent_blockhash)
                .context("building and signing ER transaction")?;
        self.wiretap_bhfa_guard(&tx, blockhash_plan_opt.as_ref())?;
        let base64_tx = encode_transaction_base64(&tx)
            .context("encoding ER transaction for router submission")?;

        if run_simulation {
            let sim_request_id = next_router_request_id();
            let sim_ctx = rpc_context("simulateTransaction", &final_submit_host, sim_request_id);
            let sim_body = serde_json::json!({
                "jsonrpc":"2.0","id":sim_request_id,"method":"simulateTransaction",
                "params":[ base64_tx.clone(), {
                    "encoding":"base64",
                    "replaceRecentBlockhash": false,
                    "sigVerify": true,
                    "commitment": "processed"
                }]
            });
            let sim_ts = Self::wiretap_dp_timestamp();
            let sim_req_name =
                Self::wiretap_dp_filename("simulateTransaction", &sim_ts, "req.json");
            self.wiretap_write_json(&sim_req_name, &sim_body);
            let sim_api_key = if Self::same_origin(&final_submit_host, &self.cfg.router_url) {
                self.data_plane_api_key_header()
            } else {
                None
            };
            let sim = self
                .post_json(&final_submit_host, &sim_body, sim_api_key)
                .await
                .with_context(|| sim_ctx.clone())?;
            let sim_resp_name =
                Self::wiretap_dp_filename("simulateTransaction", &sim_ts, "resp.txt");
            let sim_pretty =
                serde_json::to_string_pretty(&sim.value).unwrap_or_else(|_| sim.value.to_string());
            self.wiretap_write_string(&sim_resp_name, &sim_pretty);
            ensure_simulation_success(&sim.value).with_context(|| sim_ctx.clone())?;
            if let Some(tel) = telemetry {
                if Self::same_origin(&final_submit_host, &self.cfg.router_url) {
                    tel.record_router_usage(true, false);
                }
            }
        }

        let send_request_id = next_router_request_id();
        let send_ctx = rpc_context("sendTransaction", &final_submit_host, send_request_id);
        let mut final_send_ctx = send_ctx.clone();
        let initial_skip_preflight = match policy {
            SubmitPolicy::RouterNoPreflight => self.cfg.skip_preflight_on_router,
            SubmitPolicy::RpcSimulateThenSend => true,
            SubmitPolicy::OverrideThenRouterParityRetry => self.cfg.skip_preflight_on_override,
        };
        let send_body = serde_json::json!({
            "jsonrpc":"2.0","id":send_request_id,"method":"sendTransaction",
            "params":[ base64_tx.clone(), {
                "encoding":"base64",
                "skipPreflight": initial_skip_preflight,
                "preflightCommitment":"processed"
            }]
        });
        let send_ts = Self::wiretap_dp_timestamp();
        let send_req_name = Self::wiretap_dp_filename("sendTransaction", &send_ts, "req.json");
        self.wiretap_write_json(&send_req_name, &send_body);
        let send_start = Instant::now();
        let submit_api_key = if Self::same_origin(&final_submit_host, &self.cfg.router_url) {
            self.data_plane_api_key_header()
        } else {
            None
        };
        let mut send_response = self
            .post_json(&final_submit_host, &send_body, submit_api_key)
            .await
            .with_context(|| send_ctx.clone())?;
        let mut exec_duration = send_start.elapsed();
        let send_resp_name = Self::wiretap_dp_filename("sendTransaction", &send_ts, "resp.txt");
        let send_pretty = serde_json::to_string_pretty(&send_response.value)
            .unwrap_or_else(|_| send_response.value.to_string());
        self.wiretap_write_string(&send_resp_name, &send_pretty);

        let mut resolved_submit_host = final_submit_host.clone();
        let parity_retry_possible = matches!(policy, SubmitPolicy::OverrideThenRouterParityRetry);
        let mut parity_retry = false;

        if parity_retry_possible {
            if let Some(err) = send_response.value.get("error") {
                if is_blockhash_domain_error(err) {
                    warn!(
                        target: "er",
                        error_class = "host_parity",
                        bhfa_host = bhfa_host.as_str(),
                        submit_host = final_submit_host.as_str(),
                        retry_host = self.cfg.router_url.as_str(),
                        "sendTransaction returned blockhash-not-found; retrying via router"
                    );
                    if let Some(tel) = telemetry {
                        tel.record_parity_retry_router();
                    }
                    let retry_request_id = next_router_request_id();
                    let retry_ctx =
                        rpc_context("sendTransaction", &self.cfg.router_url, retry_request_id);
                    let retry_body = serde_json::json!({
                        "jsonrpc":"2.0","id":retry_request_id,"method":"sendTransaction",
                        "params":[ base64_tx.clone(), {
                            "encoding":"base64",
                            "skipPreflight": true,
                            "preflightCommitment":"processed"
                        }]
                    });
                    let retry_ts = Self::wiretap_dp_timestamp();
                    let retry_req_name =
                        Self::wiretap_dp_filename("sendTransaction", &retry_ts, "retry-req.json");
                    self.wiretap_write_json(&retry_req_name, &retry_body);
                    let router_send = self
                        .post_json(
                            &self.cfg.router_url,
                            &retry_body,
                            self.data_plane_api_key_header(),
                        )
                        .await
                        .with_context(|| retry_ctx.clone())?;
                    final_send_ctx = retry_ctx;
                    exec_duration = send_start.elapsed();
                    resolved_submit_host = self.cfg.router_url.clone();
                    preflight_skipped = true;
                    parity_retry = true;
                    let retry_resp_name =
                        Self::wiretap_dp_filename("sendTransaction", &retry_ts, "retry-resp.txt");
                    let retry_pretty = serde_json::to_string_pretty(&router_send.value)
                        .unwrap_or_else(|_| router_send.value.to_string());
                    self.wiretap_write_string(&retry_resp_name, &retry_pretty);
                    send_response = router_send;
                    info!(
                        target: "er.submit",
                        host = resolved_submit_host.as_str(),
                        reason = "router_retry_on_parity",
                        submit_policy = policy.as_str(),
                        preflight_skipped = true,
                        parity_retry = true,
                        "ER parity retry routed via Magic Router"
                    );
                }
            }
        }

        if let Some(err) = send_response.value.get("error") {
            if self.cfg.endpoint_override.is_some() && is_blockhash_not_found(err) {
                let fallback = !self.cfg.require_router;
                warn!(
                    target: "er",
                    error_class = "host_parity",
                    bhfa_host = bhfa_host.as_str(),
                    submit_host = resolved_submit_host.as_str(),
                    fallback,
                    "sendTransaction returned blockhash-not-found under endpoint override"
                );
                if self.cfg.require_router {
                    return Err(anyhow!(
                        "sendTransaction blockhash mismatch under endpoint override"
                    ))
                    .with_context(|| send_ctx.clone());
                }
            }
            Err(anyhow!("sendTransaction RPC error: {err}"))
                .with_context(|| final_send_ctx.clone())?;
        }

        let signature = send_response
            .value
            .get("result")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("sendTransaction missing string result"))
            .with_context(|| final_send_ctx.clone())?
            .to_string();

        info!(
            target: "er.submit",
            host = resolved_submit_host.as_str(),
            policy = policy.as_str(),
            parity_retry,
            sig = %signature,
            "ER transaction submitted"
        );

        if let Some(tel) = telemetry {
            if Self::same_origin(&resolved_submit_host, &self.cfg.router_url) {
                tel.record_router_usage(false, true);
            }
        }

        Ok(ErOutput {
            settlement_instructions: instructions,
            settlement_accounts: Vec::new(),
            plan_fingerprint: Some(compute_plan_fingerprint(intents)),
            exec_duration,
            blockhash_plan: blockhash_plan_opt,
            signature: Some(signature),
            preflight_skipped,
            submit_policy: policy,
            final_submit_host: resolved_submit_host,
            parity_retry,
        })
    }

    #[allow(dead_code)]
    async fn get_latest_blockhash(&self, endpoint: &Url) -> Result<Hash> {
        let request_id = next_router_request_id();
        let ctx_label = rpc_context("getLatestBlockhash", endpoint, request_id);

        if self.cfg.blockhash_cache_ttl > Duration::ZERO {
            if let Ok(mut guard) = self.blockhash_cache.lock() {
                if let Some(entry) = guard.as_ref() {
                    if entry.fetched_at.elapsed() <= self.cfg.blockhash_cache_ttl {
                        if let Some(tel) = &self.telemetry {
                            tel.record_blockhash_cache(true);
                        }
                        return Ok(entry.hash);
                    }
                }
                *guard = None;
            } else {
                warn!(target: "er", "blockhash cache mutex poisoned; bypassing cache");
            }
        }

        // Try both modern and legacy shapes.
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":request_id,"method":"getLatestBlockhash",
            "params":[{ "commitment":"processed" }]
        });
        let resp = self
            .post_json(endpoint, &body, self.data_plane_api_key_header())
            .await
            .with_context(|| ctx_label.clone())?;

        // Accept both:
        // 1) {"result":{"value":{"blockhash": "..."}}, ...}
        // 2) {"result":{"blockhash": "..."}, ...}
        let val = &resp.value;
        let hash_str = val
            .get("result")
            .and_then(|r| {
                r.get("value")
                    .and_then(|v| v.get("blockhash"))
                    .or_else(|| r.get("blockhash"))
            })
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow!("getLatestBlockhash: unexpected JSON payload"))
            .with_context(|| ctx_label.clone())?;

        Hash::from_str(hash_str)
            .map_err(|e| anyhow!("blockhash parse: {e}"))
            .with_context(|| ctx_label.clone())
            .inspect(|hash| {
                if self.cfg.blockhash_cache_ttl > Duration::ZERO {
                    if let Ok(mut guard) = self.blockhash_cache.lock() {
                        *guard = Some(BlockhashCacheEntry {
                            hash: *hash,
                            fetched_at: Instant::now(),
                        });
                    }
                    if let Some(tel) = &self.telemetry {
                        tel.record_blockhash_cache(false);
                    }
                }
            })
    }
}

#[async_trait]
impl ErClient for ErHttpClientInner {
    async fn begin_session(&self, accounts: &[Pubkey]) -> Result<ErSession> {
        if accounts.is_empty() {
            bail!("begin_session requires at least one account");
        }
        self.begin_session_router(accounts).await
    }

    // async fn execute(&self, _session: &ErSession, _intents: &[UserIntent]) -> Result<ErOutput> {
    //     bail!(
    //         "ER control-plane execute is not used; route standard Solana JSON-RPC via the Magic Router."
    //     )
    // }

    async fn execute(&self, session: &ErSession, intents: &[UserIntent]) -> Result<ErOutput> {
        let exec_start = Instant::now();
        let telemetry = self.telemetry.clone();
        let result = self
            .execute_router(session, intents, telemetry.as_ref())
            .await;

        if let Some(tel) = &telemetry {
            tel.record_execute(Some(exec_start.elapsed()), result.is_ok());
        }

        result
    }

    async fn end_session(&self, _session: &ErSession) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct RouterRpcRoute {
    fqdn: String,
    #[allow(dead_code)]
    #[serde(default)]
    identity: Option<String>,
    #[serde(rename = "ttlMs", default)]
    ttl_ms: Option<u64>,
    #[serde(default)]
    privacy: Option<String>,
}

impl RouterRpcRoute {
    fn matches_privacy(&self, mode: ErPrivacyMode) -> bool {
        match self.privacy.as_deref() {
            Some(value) => value.eq_ignore_ascii_case(mode.as_str()),
            None => true,
        }
    }

    fn ttl_duration(&self) -> Option<Duration> {
        self.ttl_ms.map(Duration::from_millis)
    }
}

#[derive(Debug, Deserialize)]
struct RouterRpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Clone)]
pub struct RouterJsonRpcError {
    pub code: i64,
    pub message: String,
}

impl RouterJsonRpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for RouterJsonRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "router json-rpc error (code {}): {}",
            self.code, self.message
        )
    }
}

impl Error for RouterJsonRpcError {}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct BlockhashRpcResult {
    blockhash: String,
    #[serde(rename = "lastValidBlockHeight")]
    last_valid_block_height: u64,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct DataPlaneBeginResult {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "blockhashPlan")]
    blockhash_plan: Option<DataPlaneBlockhashPlan>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
struct DataPlaneBlockhashPlan {
    blockhash: String,
    #[serde(rename = "lastValidSlot")]
    last_valid_slot: u64,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[derive(Default)]
struct DataPlaneExecuteMetrics {
    #[serde(rename = "cuUsed")]
    cu_used: Option<u64>,
    #[serde(rename = "fee")]
    fee_lamports: Option<u64>,
    #[serde(rename = "elapsedMs")]
    elapsed_ms: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct DataPlaneExecuteResult {
    #[serde(rename = "settlementInstructions")]
    settlement_instructions: Option<Vec<InstructionSer>>,
    #[serde(rename = "settlementAccounts")]
    settlement_accounts: Option<Vec<String>>,
    #[serde(rename = "planFingerprint")]
    plan_fingerprint: Option<String>,
    #[serde(rename = "blockhashPlan")]
    blockhash_plan: Option<DataPlaneBlockhashPlan>,
    #[serde(default)]
    metrics: DataPlaneExecuteMetrics,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouterErrorKind {
    Timeout,
    Dns,
    Tls,
    Connect,
    Request, // exposed as "http" in as_str()
    Other,
}

impl RouterErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RouterErrorKind::Timeout => "timeout",
            RouterErrorKind::Dns => "dns",
            RouterErrorKind::Tls => "tls",
            RouterErrorKind::Connect => "connect",
            RouterErrorKind::Request => "http",
            RouterErrorKind::Other => "other",
        }
    }
}

pub fn json_rpc_code_meaning(code: i64) -> &'static str {
    match code {
        -32700 => "Parse error",
        -32600 => "Invalid Request",
        -32601 => "Method not found",
        -32602 => "Invalid params",
        -32603 => "Internal error",
        -32099..=-32000 => "Server error",
        _ => "Unknown error",
    }
}

fn is_retryable_jsonrpc(code: i64) -> bool {
    !matches!(code, -32600 | -32601 | -32700)
}

#[derive(Debug)]
struct RouterRequestError {
    kind: RouterErrorKind,
    message: String,
}

impl RouterRequestError {
    fn new(kind: RouterErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn kind(&self) -> RouterErrorKind {
        self.kind
    }
}

impl fmt::Display for RouterRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "router request error (class={}): {}",
            self.kind.as_str(),
            self.message
        )
    }
}

impl Error for RouterRequestError {}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_program::{pubkey::Pubkey, system_instruction};
    use url::Url;

    #[test]
    fn plan_fingerprint_empty_is_zero() {
        assert_eq!(compute_plan_fingerprint(&[]), ZERO32);
    }

    #[test]
    fn plan_fingerprint_is_order_sensitive() {
        let payer = Pubkey::new_unique();
        let dest_a = Pubkey::new_unique();
        let dest_b = Pubkey::new_unique();
        let intent_a = UserIntent::new(
            payer,
            system_instruction::transfer(&payer, &dest_a, 10),
            1,
            None,
        );
        let intent_b = UserIntent::new(
            payer,
            system_instruction::transfer(&payer, &dest_b, 20),
            1,
            None,
        );

        let fp_ab = compute_plan_fingerprint(&[intent_a.clone(), intent_b.clone()]);
        let fp_ba = compute_plan_fingerprint(&[intent_b, intent_a]);
        assert_ne!(fp_ab, fp_ba);
    }

    #[test]
    fn intents_to_instructions_empty_rejected() {
        let err = intents_to_instructions(&[]).expect_err("expected empty intents to error");
        assert!(err.to_string().contains("no intents"));
    }

    #[test]
    fn simulate_error_surface_contains_context() {
        let err_json = serde_json::json!({
            "result": {
                "err": "InstructionError"
            }
        });
        let err = ensure_simulation_success(&err_json).unwrap_err();
        assert!(err
            .to_string()
            .contains("simulateTransaction program error"));
    }

    #[test]
    fn url_normalization_trims_trailing_slash() {
        let with_slash = Url::parse("https://example.com/endpoint/").unwrap();
        let without = Url::parse("https://example.com/endpoint").unwrap();
        assert_eq!(
            with_slash.as_str().trim_end_matches('/'),
            without.as_str().trim_end_matches('/')
        );
    }
}

fn compute_router_backoff(attempt_idx: usize) -> Duration {
    use rand::Rng;

    let base_ms = 100u64.saturating_mul(1u64 << attempt_idx.min(4));
    let jitter = rand::thread_rng().gen_range(0..=(base_ms / 4).max(1));
    let capped = (base_ms + jitter).min(1_600);
    Duration::from_millis(capped)
}

fn retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.max(1)))
}

fn retry_backoff(attempt_idx: usize, headers: &HeaderMap) -> Duration {
    retry_after_delay(headers).unwrap_or_else(|| compute_router_backoff(attempt_idx))
}

fn classify_reqwest_error(err: &reqwest::Error) -> RouterErrorKind {
    if err.is_timeout() {
        return RouterErrorKind::Timeout;
    }
    if err.is_connect() {
        let messages = error_chain_strings(err);
        return classify_error_messages(&messages);
    }
    if err.is_request() {
        return RouterErrorKind::Request;
    }
    RouterErrorKind::Other
}

fn classify_error_messages(messages: &[String]) -> RouterErrorKind {
    for msg in messages {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("dns")
            || lower.contains("failed to lookup address")
            || lower.contains("name or service not known")
            || lower.contains("no such host")
            || lower.contains("nodename nor servname provided")
            || lower.contains("temporary failure in name resolution")
        {
            return RouterErrorKind::Dns;
        }
        if lower.contains("certificate") || lower.contains("tls") || lower.contains("handshake") {
            return RouterErrorKind::Tls;
        }
    }
    RouterErrorKind::Connect
}

fn error_chain_strings(err: &reqwest::Error) -> Vec<String> {
    let mut chain = Vec::new();
    chain.push(err.to_string());
    let mut current = err.source();
    while let Some(source) = current {
        chain.push(source.to_string());
        current = source.source();
    }
    chain
}
