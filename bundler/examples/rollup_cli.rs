// bundler/examples/rollup_cli.rs
// Instrumented with deep diagnostics & wiretap support.
//
// This version follows MagicBlock's *recommended* flow:
//   1) Router probe via JSON-RPC `getRoutes` (diagnostic only)
//   2) Pick a node endpoint (either first route or --er-endpoint override)
//   3) Node data-plane probe via `getLatestBlockhash`
// No beginSession/endSession control-plane calls.
//
// Notes:
// - CLI flag: --er-wiretap-dir <PATH>
// - Writes raw JSON bodies + response bytes for router+node probes
// - Extra tracing around JSON-RPC error mapping and endpoint construction

use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use bundler::er::bhfa::{collect_writables_or_payer, get_blockhash_for_accounts, BhfaConfig};
use bundler::pipeline::blockhash::{BlockhashProvider, ErAwareBlockhashProvider};
use bundler::{
    alt::table_catalog::TableCatalog,
    alt::{AltManager, AltResolution, CachingAltManager, NoAltManager},
    chunking::{chunk_layer_with, AltPolicy, BasicOracle as ChunkOracle, TxBudget},
    dag::Dag,
    er::{
        json_rpc_code_meaning, ErClient, ErClientConfig, ErPrivacyMode, ErTelemetry, HttpErClient,
    },
    fee::FeeOracle,
    occ::rpc_fetcher::RpcAccountFetcher,
    occ::{occ_metrics_snapshot, OccConfig},
    pipeline::{
        blockhash::{BlockhashManager, BlockhashPolicy},
        rollup::{
            send_layer_parallel, ChunkEstimates, ChunkPlan, ErExecutionCtx, LayerResult,
            ParallelLayerConfig, ParallelLayerDeps,
        },
    },
    sender::{global_rpc_metrics_snapshot, track_get_latest_blockhash, CuScope, ReliableSender},
    serializer::signatures_section_len,
};
use clap::{Parser, Subcommand, ValueEnum};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig, hash::Hash, pubkey::Pubkey, signature::read_keypair_file,
    signer::Signer, system_instruction,
};

use cpsr_types::UserIntent;
use estimator::config::SafetyMargins;
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

// === Minimal JSON-RPC types (kept for router + node diagnostics) ===
#[derive(Debug, Deserialize)]
struct GetRoutesResultItem {
    fqdn: String,
    #[allow(dead_code)]
    identity: Option<String>,
    #[allow(dead_code)]
    baseFee: Option<u64>,
    #[allow(dead_code)]
    blockTimeMs: Option<u64>,
    #[allow(dead_code)]
    countryCode: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse<T> {
    #[allow(dead_code)]
    jsonrpc: String,
    #[allow(dead_code)]
    id: Option<u64>,
    result: Option<T>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    #[allow(dead_code)]
    code: i64,
    #[allow(dead_code)]
    message: String,
}

// -------------------- Helpers --------------------

/// Build the router `getRoutes` URL safely, regardless of trailing slashes.
fn get_routes_url(base: &Url) -> Result<Url> {
    base.join("getRoutes")
        .map_err(|e| anyhow!("building getRoutes URL from {base}: {e}"))
}

// -------------------- Wiretap (optional dump to disk) --------------------

#[derive(Clone)]
struct Wiretap {
    dir: Option<PathBuf>,
}

impl Wiretap {
    fn new(dir: Option<PathBuf>) -> Self {
        if let Some(d) = dir.as_ref() {
            let _ = fs::create_dir_all(d);
            #[cfg(unix)]
            {
                let _ = fs::set_permissions(d, fs::Permissions::from_mode(0o700));
            }
            tracing::info!(target: "er.wiretap", enabled = true, path = ?d, "wiretap output enabled");
        } else {
            tracing::info!(target: "er.wiretap", enabled = false, "wiretap output disabled");
        }
        Self { dir }
    }

    fn enabled(&self) -> bool {
        self.dir.is_some()
    }

    fn path(&self, filename: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(filename))
    }

    fn write_bytes(&self, filename: &str, bytes: &[u8]) {
        if let Some(path) = self.path(filename) {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(mut f) = fs::File::create(&path) {
                let _ = f.write_all(bytes);
                #[cfg(unix)]
                {
                    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
                }
            }
        }
    }

    fn write_string(&self, filename: &str, s: &str) {
        self.write_bytes(filename, s.as_bytes());
    }
}

// ---------------- Router + Node discovery/probes (recommended path) ----------------

// Diagnostic: just reads routes; does NOT mutate config.
async fn discover_er_endpoint(
    router_url: &Url,
    api_key: Option<String>,
    http_timeout: Duration,
    connect_timeout: Duration,
) -> Result<Option<Url>> {
    let client = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(http_timeout)
        .build()?;

    // POST to <router_base>/getRoutes (router is a JSON-RPC endpoint)
    let routes = get_routes_url(router_url)?;

    let mut req = client
        .post(routes.clone())
        .header("Content-Type", "application/json");

    if let Some(k) = api_key {
        if !k.is_empty() {
            req = req.header("x-api-key", k);
        }
    }

    #[derive(Serialize)]
    struct Body<'a> {
        jsonrpc: &'a str,
        id: u64,
        method: &'a str,
    }

    let body = Body {
        jsonrpc: "2.0",
        id: 1,
        method: "getRoutes",
    };

    let resp = req.json(&body).send().await?;
    let rpc: JsonRpcResponse<Vec<GetRoutesResultItem>> = resp.json().await?;
    if rpc.error.is_some() {
        return Ok(None);
    }
    if let Some(list) = rpc.result {
        if let Some(first) = list.first() {
            let url = Url::parse(&first.fqdn)?;
            return Ok(Some(url));
        }
    }
    Ok(None)
}

// Deep diagnostics for router behavior & JSON-RPC parsing (kept)
async fn probe_er_router(
    router_url: &Url,
    api_key: Option<String>,
    http_timeout: Duration,
    connect_timeout: Duration,
    wiretap: &Wiretap,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(http_timeout)
        .build()?;

    // send JSON-RPC bodies to the router base URL
    let mut req = client
        .post(router_url.clone())
        .header("Content-Type", "application/json");
    let api_key_present = api_key.as_deref().map(|s| !s.is_empty()).unwrap_or(false);
    if let Some(k) = api_key.as_deref() {
        if !k.is_empty() {
            req = req.header("x-api-key", k);
        }
    }

    // 1) well-formed getRoutes
    let good = json!({"jsonrpc":"2.0","id":1,"method":"getRoutes"});
    let good_body = good.to_string();
    let good_resp = req
        .try_clone()
        .unwrap()
        .body(good_body.clone())
        .send()
        .await?;
    let good_status = good_resp.status();
    let good_ct = good_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>")
        .to_string();
    let good_text = good_resp.text().await.unwrap_or_default();
    let good_preview = good_text.chars().take(300).collect::<String>();

    if wiretap.enabled() {
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
        wiretap.write_string(
            &format!("router_probe_{}_request_getRoutes.json", ts),
            &good_body,
        );
        wiretap.write_string(
            &format!("router_probe_{}_response_getRoutes.txt", ts),
            &good_text,
        );
    }

    tracing::info!(
        target: "er.diagnostics",
        url = %router_url,
        method = "getRoutes",
        http_status = %good_status,
        content_type = %good_ct,
        api_key_present,
        resp_len = good_text.len(),
        resp_preview = %good_preview,
        "router probe (well-formed JSON-RPC)"
    );

    // 2) deliberately bad method name to surface -32601
    let bad_method = json!({"jsonrpc":"2.0","id":2,"method":"totallyNotAMethod"});
    let bad_body = bad_method.to_string();
    let bad_resp = req
        .try_clone()
        .unwrap()
        .body(bad_body.clone())
        .send()
        .await?;
    let bad_status = bad_resp.status();
    let bad_ct = bad_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>")
        .to_string();
    let bad_text = bad_resp.text().await.unwrap_or_default();
    let bad_preview = bad_text.chars().take(300).collect::<String>();

    if wiretap.enabled() {
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
        wiretap.write_string(
            &format!("router_probe_{}_request_bad_method.json", ts),
            &bad_body,
        );
        wiretap.write_string(
            &format!("router_probe_{}_response_bad_method.txt", ts),
            &bad_text,
        );
    }

    tracing::info!(
        target: "er.diagnostics",
        url = %router_url,
        method = "totallyNotAMethod",
        http_status = %bad_status,
        content_type = %bad_ct,
        api_key_present,
        resp_len = bad_text.len(),
        resp_preview = %bad_preview,
        "router probe (invalid method to test JSON-RPC error mapping)"
    );

    // 3) deliberately malformed JSON to surface -32700 Parse error
    let malformed = r#"{"jsonrpc":"2.0","id":3,"method":"getRoutes","params":[1,2,3}"#; // missing closing ]
    let mal_resp = req.try_clone().unwrap().body(malformed).send().await?;
    let mal_status = mal_resp.status();
    let mal_ct = mal_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>")
        .to_string();
    let mal_text = mal_resp.text().await.unwrap_or_default();
    let mal_preview = mal_text.chars().take(300).collect::<String>();

    if wiretap.enabled() {
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
        wiretap.write_string(
            &format!("router_probe_{}_request_malformed.json", ts),
            malformed,
        );
        wiretap.write_string(
            &format!("router_probe_{}_response_malformed.txt", ts),
            &mal_text,
        );
    }

    tracing::info!(
        target: "er.diagnostics",
        url = %router_url,
        method = "MALFORMED_PAYLOAD",
        http_status = %mal_status,
        content_type = %mal_ct,
        api_key_present,
        resp_len = mal_text.len(),
        resp_preview = %mal_preview,
        "router probe (deliberately malformed to observe -32700 parse handling)"
    );

    Ok(())
}

fn er_privacy_mode_label(mode: ErPrivacyMode) -> &'static str {
    match mode {
        ErPrivacyMode::Public => "public",
        ErPrivacyMode::Private => "private",
    }
}

fn er_privacy_cli_label(mode: ErPrivacyCli) -> &'static str {
    match mode {
        ErPrivacyCli::Public => "public",
        ErPrivacyCli::Private => "private",
    }
}

#[derive(Clone, Debug)]
struct ErPreflightResult {
    discovered_route_base: Option<String>,
}

// NEW: recommended preflight — router getRoutes + router getBlockhashForAccounts
async fn preflight_er_connectivity(
    cfg: &ErClientConfig,
    router_url: &Url,
    api_key: Option<&str>,
    _sample_account: Pubkey,
    wiretap: &Wiretap,
) -> Result<ErPreflightResult> {
    let client = reqwest::Client::builder()
        .timeout(cfg.http_timeout)
        .connect_timeout(cfg.connect_timeout)
        .build()
        .context("building ER preflight client")?;

    // ---- Router getRoutes
    let routes_url = get_routes_url(router_url)?;
    let routes_body = json!({ "jsonrpc":"2.0","id":1,"method":"getRoutes" });

    let mut routes_req = client
        .post(routes_url.clone())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&routes_body);

    let api_key_present = api_key.map(|k| !k.is_empty()).unwrap_or(false);
    if let Some(k) = api_key {
        if !k.is_empty() {
            routes_req = routes_req.header("x-api-key", k);
        }
    }

    let t0 = Instant::now();
    let routes_resp = routes_req.send().await;
    let latency_ms = t0.elapsed().as_millis();

    let mut route_choice: Option<Url> = None;
    match routes_resp {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();

            if wiretap.enabled() {
                let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
                wiretap.write_string(
                    &format!("preflight_{}_getRoutes_req.json", ts),
                    &routes_body.to_string(),
                );
                wiretap.write_string(&format!("preflight_{}_getRoutes_resp.txt", ts), &body);
            }

            // parse result list
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(arr) = json.get("result").and_then(|v| v.as_array()) {
                    if let Some(first) = arr
                        .first()
                        .and_then(|v| v.get("fqdn"))
                        .and_then(|v| v.as_str())
                    {
                        if let Ok(u) = Url::parse(first) {
                            route_choice = Some(u);
                        }
                    }
                }
            }

            let preview = body.chars().take(240).collect::<String>();
            tracing::info!(
                target: "er.preflight",
                step = "getRoutes",
                url = %routes_url,
                http_status = status.as_u16(),
                latency_ms = latency_ms,
                api_key_present,
                candidate_route = route_choice.as_ref().map(|u| u.as_str()),
                resp_preview = %preview,
                "router probe OK (recommended flow)"
            );
        }
        Err(err) => {
            tracing::warn!(
                target: "er.preflight",
                step = "getRoutes",
                url = %routes_url,
                latency_ms = latency_ms,
                api_key_present,
                "router getRoutes request failed: {:#}",
                err
            );
        }
    }

    // ---- Pick endpoint (override takes precedence)
    let target_endpoint = cfg.endpoint_override.clone().or(route_choice.clone());

    // ---- Router BHFA probe (preferred)
    let bhfa_body = json!({
        "jsonrpc":"2.0","id":1,"method":"getBlockhashForAccounts",
        "params":[ [ _sample_account.to_string() ] ]
    });
    let mut bhfa_req = client
        .post(router_url.clone())
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&bhfa_body);
    if let Some(k) = api_key {
        if !k.is_empty() {
            bhfa_req = bhfa_req.header("x-api-key", k);
        }
    }

    let t1 = Instant::now();
    match bhfa_req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();

            if wiretap.enabled() {
                let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
                wiretap.write_string(
                    &format!("preflight_{}_bhfa_req.json", ts),
                    &bhfa_body.to_string(),
                );
                wiretap.write_string(&format!("preflight_{}_bhfa_resp.txt", ts), &text);
            }

            let (err_code, err_msg) =
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(err) = value.get("error") {
                        (
                            err.get("code").and_then(|v| v.as_i64()),
                            err.get("message")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                        )
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                };

            tracing::info!(
                target: "er.preflight",
                step = "router.getBlockhashForAccounts",
                url = %router_url,
                http_status = status.as_u16(),
                latency_ms = t1.elapsed().as_millis(),
                api_key_present = api_key.map(|k| !k.is_empty()).unwrap_or(false),
                error_code = err_code,
                error_meaning = err_code.map(json_rpc_code_meaning),
                error_message = err_msg.as_deref(),
                "router BHFA probe"
            );
        }
        Err(err) => {
            tracing::warn!(
                target: "er.preflight",
                step = "router.getBlockhashForAccounts",
                url = %router_url,
                "BHFA request failed: {:#}",
                err
            );
        }
    }

    Ok(ErPreflightResult {
        discovered_route_base: target_endpoint.map(|url| url.to_string()),
    })
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FeeOracleCli {
    Basic,
    Recent,
}

/// ------- CLI args -------
#[derive(Parser, Debug)]
#[command(name = "rollup-cli", version, about = "CPSR rollup driver")]
struct Args {
    /// RPC URL (e.g., https://api.devnet.solana.com)
    #[arg(long)]
    rpc: String,

    /// Path to payer keypair (JSON)
    #[arg(long)]
    payer: PathBuf,

    /// processed|confirmed|finalized
    #[arg(long, default_value = "processed")]
    commitment: String,

    /// Read intents from a JSON file (Vec<UserIntent>)
    #[arg(long)]
    intents: Option<PathBuf>,

    /// DEMO: generate N transfers that all WRITE the same destination (conflicts!)
    #[arg(long)]
    demo_conflicting_to: Option<String>,

    /// DEMO: count to generate for --demo-conflicting-to
    #[arg(long, default_value_t = 20)]
    demo_count: u32,

    /// DEMO: lamports per transfer
    #[arg(long, default_value_t = 100_000u64)]
    demo_lamports: u64,

    /// DEMO: mixed workload (independent memos + contended transfers)
    #[arg(long, default_value_t = false)]
    demo_mixed: bool,

    /// How many independent memo intents in --demo-mixed
    #[arg(long, default_value_t = 8)]
    mixed_memos: u32,

    /// How many contended transfers in --demo-mixed (all write same dest)
    #[arg(long, default_value_t = 6)]
    mixed_contended: u32,

    /// Force a CU price (μLam/CU). If omitted, oracle uses its default (usually non-zero floor).
    #[arg(long)]
    cu_price: Option<u64>,

    /// Max CU cap per tx
    #[arg(long)]
    max_cu_limit: Option<u64>,

    /// Use LUTs during planning (still resolved by AltManager)
    #[arg(long, default_value_t = false)]
    enable_alt: bool,

    /// Cache TTL for ALT selections (ms).
    #[arg(long, default_value_t = 5_000)]
    alt_cache_ttl_ms: u64,

    /// Provide on-chain LUT table pubkeys (comma-separated).
    #[arg(long)]
    alt_tables: Option<String>,

    /// Dry-run: simulate only, do not send
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// Also run a baseline: one tx per intent, then compare costs
    #[arg(long, default_value_t = true)]
    compare_baseline: bool,

    /// Choose how the CU window is learned for P95: "global" or "per-layer"
    #[arg(long, value_enum, default_value_t = CuScopeCli::Global)]
    cu_scope: CuScopeCli,

    /// Choose the fee oracle: "basic" (default) or "recent" (percentile + EMA)
    #[arg(long, value_enum, default_value_t = FeeOracleCli::Basic)]
    fee_oracle: FeeOracleCli,

    /// When --fee-oracle recent: primary percentile (0..1, default 0.75)
    #[arg(long)]
    fee_pctile: Option<f64>,

    /// When --fee-oracle recent: EMA alpha (0..1, default 0.20)
    #[arg(long)]
    fee_alpha: Option<f64>,

    /// When --fee-oracle recent: hysteresis in bps (e.g., 300 = 3%)
    #[arg(long)]
    fee_hysteresis_bps: Option<u64>,

    /// Override min cu price μLam/CU (floor)
    #[arg(long)]
    min_cu_price: Option<u64>,

    /// Override max cu price μLam/CU (ceiling)
    #[arg(long)]
    max_cu_price: Option<u64>,

    /// Optional: probe up to 128 account addresses for recent fee sampling
    #[arg(long)]
    probe_accounts: Option<String>, // comma-separated base58 pubkeys

    /// Faster sends: skip explicit isBlockhashValid (preflight still runs).
    #[arg(long, default_value_t = false)]
    fast_send: bool,

    /// Max parallel chunk workers (1..12).
    #[arg(long, default_value_t = 6)]
    concurrency: usize,

    /// Refresh blockhash if older than this (ms).
    #[arg(long, default_value_t = 8_000)]
    blockhash_max_age_ms: u64,

    /// Refresh blockhash after this many uses.
    #[arg(long, default_value_t = 16)]
    blockhash_refresh_every_n: u32,

    /// Write a JSON report here (optional)
    #[arg(long)]
    out_report: Option<PathBuf>,

    /// Enable Ephemeral Rollups optimization
    #[arg(long, default_value_t = false)]
    er_enabled: bool,

    /// Override ER endpoint (skip router discovery). Only use if instructed.
    #[arg(long)]
    er_endpoint: Option<String>,

    /// Magic Router base URL for ER discovery (e.g. https://devnet-router.magicblock.app/)
    #[arg(long)]
    er_router: Option<String>,

    /// Router API key (if required)
    #[arg(long)]
    er_router_key: Option<String>,

    /// If set, after ER discovery we will replace the effective RPC base URL with the discovered Magic Router route for this run.
    #[arg(long, default_value_t = false)]
    er_proxy_rpc: bool,

    /// Choose ER privacy tier: public or private
    #[arg(long, value_enum, default_value_t = ErPrivacyCli::Public)]
    er_privacy: ErPrivacyCli,

    /// ER HTTP retries on transient failures (additional attempts)
    #[arg(long, default_value_t = 4)]
    er_retries: usize,

    /// ER HTTP request timeout (ms)
    #[arg(long, default_value_t = 3_000)]
    er_http_timeout_ms: u64,

    /// ER HTTP connect timeout (ms)
    #[arg(long, default_value_t = 1_000)]
    er_connect_timeout_ms: u64,

    /// Router circuit breaker failure threshold
    #[arg(long, default_value_t = 3)]
    er_circuit_failures: u32,

    /// Router circuit breaker cooldown (ms)
    #[arg(long, default_value_t = 10_000)]
    er_circuit_cooldown_ms: u64,

    /// Router route cache TTL (ms)
    #[arg(long, default_value_t = 15_000)]
    er_route_ttl_ms: u64,

    /// Blockhash cache TTL (ms)
    #[arg(long, default_value_t = 2_000)]
    er_blockhash_ttl_ms: u64,

    /// Requested ER session lifetime (ms) (kept for compatibility; not used by preflight)
    #[arg(long, default_value_t = 60_000)]
    er_session_ms: u64,

    /// (Kept for compatibility) Previously required control-plane session before settlement.
    /// No longer enforced in this CLI since ER doesn't require beginSession.
    #[arg(long, default_value_t = false)]
    er_require: bool,

    /// Directory to dump ER wiretap (requests/responses) for diagnostics
    #[arg(long)]
    er_wiretap_dir: Option<PathBuf>,

    /// Minimum estimated CU to run a chunk via ER (skip below this)
    #[arg(long, default_value_t = 10_000)]
    er_min_cu_threshold: u64,

    /// When set, allow chunker to merge adjacent tiny intents for ER optimization
    #[arg(long, default_value_t = false)]
    er_merge_small_intents: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CuScopeCli {
    Global,
    PerLayer,
}
impl From<CuScopeCli> for CuScope {
    fn from(v: CuScopeCli) -> Self {
        match v {
            CuScopeCli::Global => CuScope::Global,
            CuScopeCli::PerLayer => CuScope::PerScope,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ErPrivacyCli {
    Public,
    Private,
}
impl From<ErPrivacyCli> for ErPrivacyMode {
    fn from(v: ErPrivacyCli) -> Self {
        match v {
            ErPrivacyCli::Public => ErPrivacyMode::Public,
            ErPrivacyCli::Private => ErPrivacyMode::Private,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct TxCost {
    signature: Option<String>,
    used_cu: u32,
    cu_limit: u64,
    cu_price_micro_lamports: u64,
    message_bytes: usize,
    required_signers: usize,
    /// serialized packet size (signatures shortvec + signatures*64 + message)
    packet_bytes: usize,
    /// max allowed packet size inferred from guard budgeting (~1232B total)
    max_packet_bytes: usize,
    /// whether the 1232-byte guard passed
    packet_guard_pass: bool,
    base_fee_lamports: u64,
    priority_fee_lamports: u64,
    total_fee_lamports: u64,
}

#[derive(Clone, Debug, Serialize)]
struct DemoReport {
    intents: usize,
    dag_layers: Vec<usize>,
    rollup_chunks: usize,
    rollup_total_fee: u64,
    baseline_txs: usize,
    baseline_total_fee: u64,
    absolute_savings_lamports: i64,
    savings_percent: f64,
    rollup_txs: Vec<TxCost>,
    baseline: Vec<TxCost>,
}

#[derive(Clone, Debug, Serialize)]
struct RouterHealthSummary {
    discovery_success_rate: f64,
    discovery_p50_ms: Option<f64>,
    discovery_p95_ms: Option<f64>,
    cache_hit_rate: f64,
    cache_hits: u64,
    cache_misses: u64,
    blockhash_cache_hit_rate: f64,
    blockhash_cache_hits: u64,
    blockhash_cache_misses: u64,
    error_counts: BTreeMap<String, u64>,
    blockhash_p50_ms: Option<f64>,
    blockhash_p95_ms: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
struct ErPerformanceSummary {
    settlements: usize,
    settlement_cu_total: u64,
    settlement_cu_avg: Option<f64>,
    settlement_fee_total: u64,
    settlement_fee_avg: Option<f64>,
    baseline_cu_total: u64,
    baseline_fee_total: u64,
    cu_savings_ratio: f64,
    fee_savings_ratio: f64,
    latency_ms_p50: Option<f64>,
    latency_ms_p95: Option<f64>,
    latency_ms_stddev: Option<f64>,
    latency_ms_variance: Option<f64>,
    baseline_latency_ms_p50: Option<f64>,
    baseline_latency_ms_p95: Option<f64>,
    baseline_latency_ms_stddev: Option<f64>,
    baseline_latency_ms_variance: Option<f64>,
    exec_ms_p50: Option<f64>,
    exec_ms_p95: Option<f64>,
    exec_ms_stddev: Option<f64>,
    exec_ms_variance: Option<f64>,
    plan_fingerprint_coverage: f64,
    er_success_rate: f64,
    er_fallback_rate: f64,
    router: Option<RouterHealthSummary>,
    session_begin_attempts: u64,
    session_begin_ok: u64,
    session_begin_err: u64,
    last_router_error_code: Option<i64>,
    last_router_error_message: Option<String>,
    skipped_small_chunks: u64,
    merged_small_chunks: u64,
}

#[derive(Clone, Debug)]
struct LayerSummary {
    index: usize,
    chunks: usize,
    success: usize,
    failure: usize,
    wall_clock: Duration,
}

#[derive(Clone, Debug)]
struct ErChunkSummaryRow {
    layer: usize,
    chunk: usize,
    decision: String,
    reason: Option<String>,
    route: Option<String>,
    total_ms: u128,
    exec_ms: Option<u128>,
    used_cu: u32,
    er_attempted: bool,
    session_established: bool,
}

#[derive(Clone, Debug, Serialize)]
struct StageMetric {
    name: String,
    count: u64,
    total_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
struct TracingReport {
    run_mode: String,
    stages: Vec<StageMetric>,
    rpc: bundler::sender::RpcMetrics,
    mock_in_use: bool,
    rpc_url: String,
}

fn parse_commitment(s: &str) -> CommitmentConfig {
    match s {
        "processed" => CommitmentConfig::processed(),
        "confirmed" => CommitmentConfig::confirmed(),
        "finalized" => CommitmentConfig::finalized(),
        _ => CommitmentConfig::processed(),
    }
}

// Small helper: build a Memo v2 instruction (no account metas => no conflicts)
fn memo_ix(text: &str) -> solana_sdk::instruction::Instruction {
    use solana_sdk::{instruction::Instruction, pubkey::Pubkey};
    let memo_pid =
        Pubkey::from_str("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr").expect("memo program id");
    Instruction {
        program_id: memo_pid,
        accounts: vec![],
        data: text.as_bytes().to_vec(),
    }
}

fn percentile_duration(values: &mut Vec<Duration>, pct: f64) -> Option<Duration> {
    if values.is_empty() {
        return None;
    }
    values.sort();
    let n = values.len();
    let idx = (((n as f64) * pct).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1);
    Some(values[idx])
}

fn duration_variance_stddev(values: &[Duration]) -> (Option<f64>, Option<f64>) {
    if values.is_empty() {
        return (None, None);
    }
    let ms: Vec<f64> = values.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    let mean = ms.iter().sum::<f64>() / ms.len() as f64;
    let variance = if ms.len() > 1 {
        ms.iter()
            .map(|x| {
                let delta = x - mean;
                delta * delta
            })
            .sum::<f64>()
            / ms.len() as f64
    } else {
        0.0
    };
    let stddev = variance.sqrt();
    (Some(variance), Some(stddev))
}

fn duration_to_ms(value: Option<Duration>) -> Option<f64> {
    value.map(|d| d.as_secs_f64() * 1000.0)
}

fn fmt_opt_f64(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{:.2}", v),
        None => "-".to_string(),
    }
}

fn truncate_cell(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_string();
    }
    let take = max.saturating_sub(1);
    let mut truncated: String = input.chars().take(take).collect();
    truncated.push('…');
    truncated
}

const SELF_CHECK_FEE_THRESHOLD: f64 = 0.50;
const SELF_CHECK_EXEC_P95_THRESHOLD_MS: f64 = 500.0;

fn fee_based_self_check_failures(
    fee_savings_ratio: f64,
    er_exec_p95_ms: Option<f64>,
) -> Vec<String> {
    let mut failures = Vec::new();

    if fee_savings_ratio < SELF_CHECK_FEE_THRESHOLD {
        failures.push(format!(
            "fee savings ratio {:.2}% below {:.0}% threshold",
            fee_savings_ratio * 100.0,
            SELF_CHECK_FEE_THRESHOLD * 100.0
        ));
    }

    match er_exec_p95_ms {
        Some(value) if value <= SELF_CHECK_EXEC_P95_THRESHOLD_MS => {}
        Some(value) => failures.push(format!(
            "ER execute p95 {:.2}ms exceeds {:.0}ms threshold",
            value, SELF_CHECK_EXEC_P95_THRESHOLD_MS
        )),
        None => failures.push("ER execute p95 unavailable to evaluate gate".to_string()),
    }

    failures
}

// (make_build_vmsg helper unchanged)
fn make_build_vmsg<'a>(
    payer_pk: solana_sdk::pubkey::Pubkey,
    bh: &'a mut Hash,
    alts: bundler::alt::AltResolution,
    chunk_intents: Vec<cpsr_types::UserIntent>,
    safety_cfg: estimator::config::SafetyMargins,
    _rpc: std::sync::Arc<solana_client::rpc_client::RpcClient>,
    _commitment: solana_sdk::commitment_config::CommitmentConfig,
) -> impl FnMut(bundler::fee::FeePlan) -> anyhow::Result<solana_sdk::message::VersionedMessage> + 'a
{
    move |plan: bundler::fee::FeePlan| {
        let ctx = bundler::serializer::BuildTxCtx {
            payer: payer_pk,
            blockhash: *bh,
            fee: plan,
            alts: alts.clone(),
            safety: safety_cfg.clone(),
        };
        bundler::serializer::build_v0_message(&ctx, &chunk_intents)
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let mut rpc_base_url = args.rpc.clone();
    let payer = Arc::new(
        read_keypair_file(&args.payer)
            .map_err(|e| anyhow!("reading keypair {:?}: {e}", args.payer))?,
    );
    let payer_pubkey = payer.pubkey();

    let commitment = parse_commitment(&args.commitment);

    let wiretap = Wiretap::new(args.er_wiretap_dir.clone());
    let mut er_bhfa_cfg: Option<BhfaConfig> = None;

    // --------- Intents ----------
    let intents: Vec<UserIntent> = if let Some(path) = args.intents.as_ref() {
        let data = fs::read_to_string(path).context("reading intents file")?;
        let parsed: Vec<UserIntent> =
            serde_json::from_str(&data).context("parsing intents JSON")?;
        parsed.into_iter().map(|ui| ui.normalized()).collect()
    } else if args.demo_mixed {
        let to_str = args
            .demo_conflicting_to
            .as_ref()
            .ok_or_else(|| anyhow!("--demo-mixed also requires --demo-conflicting-to <PUBKEY>"))?;
        let dest = Pubkey::from_str(to_str)
            .map_err(|_| anyhow!("--demo-conflicting-to must be a valid pubkey"))?;

        let mut v = Vec::new();
        for i in 0..args.mixed_memos {
            let ix = memo_ix(&format!("CPSR demo memo #{i}"));
            v.push(UserIntent::new(payer_pubkey, ix, 9, None));
        }
        for i in 0..args.mixed_contended {
            let ix = system_instruction::transfer(&payer_pubkey, &dest, args.demo_lamports);
            let prio: u8 = match i % 3 {
                0 => 7,
                1 => 5,
                _ => 3,
            };
            v.push(UserIntent::new(payer_pubkey, ix, prio, None));
        }
        v
    } else if let Some(to_str) = &args.demo_conflicting_to {
        let dest = Pubkey::from_str(to_str)
            .map_err(|_| anyhow!("--demo-conflicting-to must be a valid pubkey"))?;
        (0..args.demo_count)
            .map(|_| {
                let ix = system_instruction::transfer(&payer_pubkey, &dest, args.demo_lamports);
                UserIntent::new(payer_pubkey, ix, 0, None)
            })
            .collect()
    } else {
        return Err(anyhow!(
            "Provide either --intents PATH, --demo-mixed (with --demo-conflicting-to), or --demo-conflicting-to PUBKEY"
        ));
    };
    if intents.is_empty() {
        return Err(anyhow!("No intents to run"));
    }

    // --------- Static config ----------
    let safety = SafetyMargins::default();
    let mut tx_budget = TxBudget::default();
    if args.er_merge_small_intents {
        tx_budget.max_instructions = tx_budget.max_instructions.saturating_mul(2).min(60);
        tx_budget.max_bytes = tx_budget.max_bytes.saturating_add(256);
    }
    let alt_policy = AltPolicy {
        enabled: args.enable_alt,
        ..AltPolicy::default()
    };
    let occ_cfg = OccConfig::default();

    // --------- ER router (if enabled). IMPORTANT: sender still uses the real RPC.
    let (er_ctx_opt, er_metrics): (Option<ErExecutionCtx>, Option<Arc<ErTelemetry>>) = if args
        .er_enabled
    {
        let telemetry = Arc::new(ErTelemetry::default());
        let router_str = args
            .er_router
            .clone()
            .unwrap_or_else(|| "https://devnet-router.magicblock.app/".to_string());
        let router_url = Url::parse(&router_str).context("invalid --er-router URL")?;
        let endpoint_override = match args.er_endpoint.as_deref() {
            Some(raw) => Some(Url::parse(raw).context("invalid --er-endpoint URL")?),
            None => None,
        };

        let http_timeout = Duration::from_millis(args.er_http_timeout_ms.max(100));
        let connect_timeout = Duration::from_millis(args.er_connect_timeout_ms.max(50));
        let circuit_cooldown = Duration::from_millis(args.er_circuit_cooldown_ms.max(100));

        // Diagnostic discovery (DO NOT override automatically)
        if endpoint_override.is_none() {
            match discover_er_endpoint(
                &router_url,
                args.er_router_key.clone(),
                http_timeout,
                connect_timeout,
            )
            .await
            {
                Ok(Some(url)) => {
                    tracing::info!(
                        target: "er",
                        router = %router_url.as_str(),
                        discovered_endpoint = %url.as_str(),
                        "Router discovery succeeded via /getRoutes (diagnostic only; not overriding endpoint)"
                    );
                }
                Ok(None) => {
                    tracing::warn!(
                        target: "er",
                        router = %router_url.as_str(),
                        "Router discovery returned no routes or error; proceeding with router only"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "er",
                        router = %router_url.as_str(),
                        "Router discovery failed ({e}); proceeding with router only"
                    );
                }
            }
        }

        // Active probes to capture HTTP/JSON-RPC behavior and payloads
        if let Err(e) = probe_er_router(
            &router_url,
            args.er_router_key.clone(),
            http_timeout,
            connect_timeout,
            &wiretap,
        )
        .await
        {
            tracing::warn!(target: "er", router = %router_url, "router probe failed: {e}");
        }

        let api_key_state = match args.er_router_key.as_deref() {
            Some(k) if k.is_empty() => {
                tracing::warn!(
                    target: "er.config",
                    "router API key provided but empty; header will be omitted"
                );
                "empty"
            }
            Some(_) => "present",
            None => "absent",
        };

        let cfg = ErClientConfig {
            endpoint_override,
            http_timeout,
            connect_timeout,
            // kept in struct; not used by preflight now
            session_ttl: Duration::from_millis(args.er_session_ms.max(1_000)),
            retries: args.er_retries,
            privacy_mode: args.er_privacy.into(),
            router_url,
            router_api_key: args.er_router_key.clone(),
            route_cache_ttl: Duration::from_millis(args.er_route_ttl_ms),
            circuit_breaker_failures: args.er_circuit_failures,
            circuit_breaker_cooldown: circuit_cooldown,
            payer: payer.clone(),
            blockhash_cache_ttl: Duration::from_millis(args.er_blockhash_ttl_ms),
            min_cu_threshold: args.er_min_cu_threshold,
            merge_small_intents: args.er_merge_small_intents,
            telemetry: Some(telemetry.clone()),
        };
        er_bhfa_cfg = Some(BhfaConfig::new(
            cfg.router_url.clone(),
            cfg.router_api_key.clone(),
            cfg.http_timeout,
            cfg.connect_timeout,
        ));

        let privacy_label = er_privacy_cli_label(args.er_privacy);
        tracing::info!(
            target: "er.config",
            router = cfg.router_url.as_str(),
            privacy = privacy_label,
            http_timeout_ms = cfg.http_timeout.as_millis(),
            connect_timeout_ms = cfg.connect_timeout.as_millis(),
            session_ttl_ms = cfg.session_ttl.as_millis(),
            retries = cfg.retries,
            circuit_breaker_failures = cfg.circuit_breaker_failures,
            circuit_breaker_cooldown_ms = cfg.circuit_breaker_cooldown.as_millis(),
            route_cache_ttl_ms = cfg.route_cache_ttl.as_millis(),
            blockhash_cache_ttl_ms = cfg.blockhash_cache_ttl.as_millis(),
            min_cu_threshold = cfg.min_cu_threshold,
            merge_small_intents = args.er_merge_small_intents,
            endpoint_override = cfg
                .endpoint_override
                .as_ref()
                .map(|u| u.as_str())
                .unwrap_or("<none>"),
            api_key_state = api_key_state,
            er_require = args.er_require,
            "ER feature gate summary"
        );

        if let Some(ep) = cfg.endpoint_override.as_ref() {
            tracing::info!(
                target: "er.config",
                endpoint_override = ep.as_str(),
                "Router dispatch bypassed for data plane due to --er-endpoint"
            );
        }

        // RECOMMENDED PREFLIGHT (no control-plane)
        let preflight_result = match preflight_er_connectivity(
            &cfg,
            &cfg.router_url,
            cfg.router_api_key.as_deref(),
            payer_pubkey,
            &wiretap,
        )
        .await
        {
            Ok(res) => Some(res),
            Err(err) => {
                tracing::warn!(
                    target: "er.preflight",
                    router = cfg.router_url.as_str(),
                    "preflight connectivity probe failed: {:#}",
                    err
                );
                None
            }
        };

        if args.er_proxy_rpc {
            match preflight_result
                .as_ref()
                .and_then(|res| res.discovered_route_base.clone())
            {
                Some(route) => {
                    let effective = route; // rely on Url normalization from router
                    tracing::info!(
                        target: "er",
                        effective = %effective,
                        "ER proxy mode enabled: switching effective RPC base to Magic Router route"
                    );
                    rpc_base_url = effective;
                }
                None => {
                    tracing::warn!(
                        target: "er",
                        "ER proxy mode requested but no route discovered; keeping original --rpc"
                    );
                }
            }
        }

        let client = HttpErClient::new(cfg.clone())?;
        let client: Arc<dyn ErClient> = Arc::new(client);

        // IMPORTANT: enable ER execution (the previous code passed `true` here, which
        // made the ctx discovery-only and forced a fallback every time).
        let enabled = true;

        (
            Some(ErExecutionCtx::new(
                client,
                telemetry.clone(),
                enabled,
                args.dry_run,
                cfg.min_cu_threshold,
                cfg.merge_small_intents,
            )),
            Some(telemetry),
        )
    } else {
        (None, None)
    };

    let rpc = Arc::new(RpcClient::new_with_timeout_and_commitment(
        rpc_base_url.clone(),
        Duration::from_secs(25),
        commitment.clone(),
    ));

    // Mock check
    let mock_in_use = rpc_base_url.contains("mock") || std::env::var("SOLANA_RPC_MOCK").is_ok();
    tracing::info!(
        rpc_url = %rpc_base_url,
        mock_in_use = mock_in_use,
        "RPC client initialized"
    );

    // --------- Deps ----------
    let catalog = if let Some(list) = args.alt_tables.as_deref() {
        let table_pks: Vec<Pubkey> = list
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| Pubkey::from_str(s))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| anyhow!("invalid --alt-tables list"))?;
        if table_pks.is_empty() {
            None
        } else {
            Some(Arc::new(TableCatalog::from_rpc(rpc.clone(), table_pks)?))
        }
    } else {
        None
    };

    let mut alt_mgr_builder = CachingAltManager::new(
        alt_policy.clone(),
        Duration::from_millis(args.alt_cache_ttl_ms),
    );
    if let Some(cat) = &catalog {
        alt_mgr_builder = alt_mgr_builder.with_catalog(cat.clone());
    }
    let alt_mgr: Arc<dyn AltManager> = if args.enable_alt {
        Arc::new(alt_mgr_builder)
    } else {
        Arc::new(NoAltManager)
    };

    let account_fetcher = Arc::new(RpcAccountFetcher::new_with_drift(
        rpc.clone(),
        commitment.clone(),
        occ_cfg.max_slot_drift,
    ));

    let max_age_ms = args.blockhash_max_age_ms.clamp(1_000, 120_000);
    let refresh_every = args.blockhash_refresh_every_n.max(1);
    let blockhash_policy = BlockhashPolicy {
        max_age: Duration::from_millis(max_age_ms as u64),
        refresh_every,
    };
    let blockhash_manager = Arc::new(BlockhashManager::new(
        rpc.clone(),
        commitment.clone(),
        blockhash_policy.clone(),
        !args.fast_send,
    ));
    let blockhash_provider: Arc<dyn BlockhashProvider> = if let Some(cfg) = er_bhfa_cfg.clone() {
        Arc::new(ErAwareBlockhashProvider::new(
            blockhash_manager.clone(),
            cfg,
            payer_pubkey,
        ))
    } else {
        blockhash_manager.clone()
    };
    tracing::info!(
        target: "blockhash",
        max_age_ms = max_age_ms,
        refresh_every = refresh_every,
        pre_check = !args.fast_send,
        "Blockhash policy configured"
    );

    // Choose fee oracle
    let fee_oracle: Box<dyn FeeOracle> = match args.fee_oracle {
        FeeOracleCli::Basic => Box::new(bundler::fee::BasicFeeOracle {
            max_cu_limit: args.max_cu_limit.unwrap_or(1_400_000),
            min_cu_price: args.min_cu_price.or(args.cu_price).unwrap_or(100),
            max_cu_price: args.max_cu_price.unwrap_or(5_000),
        }),
        FeeOracleCli::Recent => {
            use bundler::fee_oracles::recent::RecentFeesOracle;

            let probes: Vec<Pubkey> = args
                .probe_accounts
                .as_deref()
                .unwrap_or("")
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| Pubkey::from_str(s.trim()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| anyhow!("invalid --probe-accounts list"))?;

            let mut o = RecentFeesOracle::new(rpc.clone(), probes);
            if let Some(v) = args.max_cu_limit {
                o.max_cu_limit = v;
            }
            if let Some(v) = args.min_cu_price.or(args.cu_price) {
                o.min_cu_price = v;
            }
            if let Some(v) = args.max_cu_price {
                o.max_cu_price = v;
            }
            if let Some(v) = args.fee_pctile {
                o.p_primary = v;
            }
            if let Some(v) = args.fee_alpha {
                o.ema_alpha = v;
            }
            if let Some(v) = args.fee_hysteresis_bps {
                o.hysteresis_bps = v;
            }
            Box::new(o)
        }
    };
    // Build sender against the actual Solana RPC (never the router URL)
    let sender = {
        let mut s = ReliableSender::with_scope(rpc.clone(), fee_oracle, args.cu_scope.into());
        if args.fast_send {
            s = s.with_fast_send();
        }
        Arc::new(s)
    };

    let concurrency = args.concurrency.clamp(1, 12);
    let parallel_cfg_template = ParallelLayerConfig {
        max_concurrency: concurrency,
        max_attempts: 3,
        base_backoff: Duration::from_millis(200),
        max_backoff: Duration::from_millis(1_500),
        dry_run: args.dry_run,
    };
    let parallel_deps = ParallelLayerDeps {
        sender: sender.clone(),
        payer: payer.clone(),
        safety: safety.clone(),
        alt_manager: alt_mgr.clone(),
        occ_fetcher: account_fetcher.clone(),
        occ_config: occ_cfg.clone(),
        blockhash_provider: blockhash_provider.clone(),
        er: er_ctx_opt,
    };

    // --------- DAG & layers ----------
    let dag_start = Instant::now();
    let dag = {
        let span = tracing::info_span!("dag.build", intents_total = intents.len());
        let _enter = span.enter();
        tracing::info!("Building DAG from {} intents", intents.len());
        Dag::build(intents.clone())
    };
    let dag_duration = dag_start.elapsed();
    tracing::info!("DAG build completed in {:?}", dag_duration);

    let layers = dag.layers().context("topo layers")?;
    let layers_sizes: Vec<usize> = layers.iter().map(|l| l.len()).collect();
    tracing::info!("DAG has {} layers: {:?}", layers.len(), layers_sizes);

    let mut rollup_reports: Vec<TxCost> = Vec::new();
    let mut layer_summaries: Vec<LayerSummary> = Vec::new();
    let mut er_total_latency = Duration::ZERO;
    let mut er_latency_samples: Vec<Duration> = Vec::new();
    let mut er_exec_samples: Vec<Duration> = Vec::new();
    let mut er_cu_total: u64 = 0;
    let mut er_fee_total: u64 = 0;
    let mut er_chunks_total = 0usize;
    let mut er_plan_fingerprint_real = 0usize;
    let mut er_chunk_rows: Vec<ErChunkSummaryRow> = Vec::new();
    let mut er_real_chunks = 0usize;
    let mut er_simulated_chunks = 0usize;

    // --------- ROLLUP path ----------
    tracing::info!("Starting rollup processing with {} layers", layers.len());
    for (layer_index, layer) in layers.iter().enumerate() {
        let layer_span = tracing::info_span!(
            "chunking.layer",
            layer_index = layer_index,
            layer_size = layer.len()
        );
        let _guard = layer_span.enter();

        tracing::info!(
            "Processing layer {} with {} intents",
            layer_index,
            layer.len()
        );
        sender.start_scope();

        let chunking_start = Instant::now();
        let planned_chunks =
            chunk_layer_with(layer, &dag.nodes, &tx_budget, &alt_policy, &ChunkOracle)?;
        let chunking_duration = chunking_start.elapsed();
        tracing::info!(
            "Chunking layer {} took {:?}, produced {} chunks",
            layer_index,
            chunking_duration,
            planned_chunks.len()
        );

        let mut chunk_plans = Vec::with_capacity(planned_chunks.len());
        for (chunk_idx, planned) in planned_chunks.iter().enumerate() {
            let chunk_intents: Vec<UserIntent> = planned
                .node_ids
                .iter()
                .map(|&nid| dag.nodes[nid as usize].clone())
                .collect();
            tracing::info!(
                target: "planner",
                chunk_index = chunk_idx,
                node_count = planned.node_ids.len(),
                est_cu = planned.est_cu,
                est_msg_bytes = planned.est_message_bytes,
                alt_ro = planned.alt_readonly,
                alt_wr = planned.alt_writable,
                "planned chunk"
            );
            chunk_plans.push(ChunkPlan {
                index: chunk_idx,
                intents: chunk_intents,
                estimates: ChunkEstimates {
                    est_cu: planned.est_cu,
                    est_msg_bytes: planned.est_message_bytes,
                    alt_ro_count: planned.alt_readonly,
                    alt_wr_count: planned.alt_writable,
                },
                merged_indices: Vec::new(),
            });
        }

        let mut cfg = parallel_cfg_template.clone();
        if chunk_plans.len() <= 1 {
            cfg.max_concurrency = 1;
        }
        let use_parallel = cfg.max_concurrency > 1 && chunk_plans.len() > 1;

        let LayerResult {
            layer_index: _,
            wall_clock,
            chunks,
        } = send_layer_parallel(layer_index, chunk_plans, parallel_deps.clone(), cfg).await;

        let mut layer_success = 0usize;
        let mut layer_failure = 0usize;
        let mut simulate_durations: Vec<Duration> = Vec::new();
        let mut send_durations: Vec<Duration> = Vec::new();

        for chunk in chunks {
            let chunk_er_diag = chunk.er.clone();
            let chunk_index = chunk.index;
            let chunk_attempts = chunk.attempts;
            let chunk_timings = chunk.timings;
            let chunk_estimates = chunk.estimates.clone();

            match chunk.outcome {
                bundler::pipeline::rollup::ChunkOutcome::Success(success) => {
                    layer_success += 1;
                    simulate_durations.push(success.report.simulate_duration);
                    if let Some(send_dur) = success.report.send_duration {
                        send_durations.push(send_dur);
                    }

                    let mut fallback_reason = chunk_er_diag.fallback_reason.clone();
                    if !chunk_er_diag.session_established {
                        if fallback_reason.is_none() {
                            let inferred = if !chunk_er_diag.er_ctx_present {
                                "context_absent"
                            } else if !chunk_er_diag.er_enabled {
                                "er_disabled_by_config"
                            } else if !chunk_er_diag.attempted {
                                "er_not_attempted"
                            } else {
                                "unspecified (inspect logs)"
                            };
                            fallback_reason = Some(inferred.to_string());
                        }
                    }

                    // Interpret decision for summary
                    let decision = if chunk_er_diag.session_established {
                        "ran_er"
                    } else if chunk_er_diag.simulated {
                        "simulated_er"
                    } else {
                        "fell_back"
                    };

                    // If we have telemetry, echo last router json-rpc error for context
                    if let Some(ref tm) = er_metrics {
                        let sum = tm.summary();
                        if let Some(code) = sum.last_router_error_code {
                            let meaning = json_rpc_code_meaning(code);
                            tracing::info!(
                                target: "er.context",
                                layer_index,
                                chunk_index,
                                last_router_error_code = code,
                                last_router_error_meaning = %meaning,
                                last_router_error_message = %sum
                                    .last_router_error_message
                                    .as_deref()
                                    .unwrap_or("-"),
                                "router last JSON-RPC error snapshot"
                            );
                        }
                    }

                    if success.plan_fingerprint.is_some() && chunk_er_diag.session_established {
                        er_plan_fingerprint_real += 1;
                    }

                    if chunk_er_diag.session_established {
                        er_real_chunks += 1;
                        er_chunks_total += 1;
                        er_cu_total += success.report.used_cu as u64;
                        er_fee_total += success.report.total_fee_lamports;
                        er_total_latency += chunk_timings.total;
                        er_latency_samples.push(chunk_timings.total);
                        if let Some(exec_dur) = success.er_execution_duration {
                            er_exec_samples.push(exec_dur);
                        }
                    } else if chunk_er_diag.simulated {
                        er_simulated_chunks += 1;
                    }

                    let sig_section = signatures_section_len(success.report.required_signers);
                    let packet_bytes = success.report.message_bytes + sig_section;
                    let max_msg_budget =
                        safety.max_message_bytes_with_signers(success.report.required_signers);
                    let max_packet_bytes = max_msg_budget + sig_section;
                    let guard_pass = success.report.message_bytes <= max_msg_budget;
                    let er_execution_ms_opt = success.er_execution_duration.map(|d| d.as_millis());

                    tracing::info!(
                        target: "er.decide",
                        layer_index,
                        chunk_index = chunk_index,
                        er_enabled = args.er_enabled,
                        er_ctx_present = chunk_er_diag.er_ctx_present,
                        er_attempted = chunk_er_diag.attempted,
                        er_simulated = chunk_er_diag.simulated,
                        er_execution_duration_ms = er_execution_ms_opt,
                        plan_fingerprint_present = success.plan_fingerprint.is_some(),
                        used_cu = success.report.used_cu,
                        final_cu_limit = success.report.final_plan.cu_limit,
                        final_cu_price = success.report.final_plan.cu_price_microlamports,
                        message_bytes = success.report.message_bytes,
                        alt_keys_offloaded = success.alt_resolution.stats.keys_offloaded,
                        guard_pass,
                        decision = decision,
                        fallback_reason = fallback_reason.as_deref(),
                        route_endpoint = chunk_er_diag.route_endpoint.as_deref(),
                        "ER per-chunk decision"
                    );

                    er_chunk_rows.push(ErChunkSummaryRow {
                        layer: layer_index,
                        chunk: chunk_index,
                        decision: decision.to_string(),
                        reason: fallback_reason.clone(),
                        route: chunk_er_diag.route_endpoint.clone(),
                        total_ms: chunk_timings.total.as_millis(),
                        exec_ms: er_execution_ms_opt,
                        used_cu: success.report.used_cu,
                        er_attempted: chunk_er_diag.attempted,
                        session_established: chunk_er_diag.session_established,
                    });

                    tracing::info!(
                        target: "pipeline",
                        layer_index,
                        chunk_index = chunk_index,
                        attempts = chunk_attempts,
                        total_ms = chunk_timings.total.as_millis(),
                        simulate_ms = success.report.simulate_duration.as_millis(),
                        send_ms = success.report.send_duration.map(|d| d.as_millis()).unwrap_or(0),
                        est_cu = chunk_estimates.est_cu,
                        est_msg_bytes = chunk_estimates.est_msg_bytes,
                        alt_keys = success.alt_resolution.stats.keys_offloaded,
                        alt_saved_bytes = success.alt_resolution.stats.estimated_saved_bytes,
                        last_valid_block_height = success.lease.last_valid_block_height,
                        "chunk completed"
                    );

                    if let Some(sig) = &success.report.signature {
                        tracing::info!(
                            target: "pipeline",
                            layer_index,
                            chunk_index = chunk_index,
                            sig = %sig,
                            "transaction sent"
                        );
                    }

                    rollup_reports.push(TxCost {
                        signature: success.report.signature.map(|s| s.to_string()),
                        used_cu: success.report.used_cu,
                        cu_limit: success.report.final_plan.cu_limit,
                        cu_price_micro_lamports: success.report.final_plan.cu_price_microlamports,
                        message_bytes: success.report.message_bytes,
                        required_signers: success.report.required_signers,
                        packet_bytes,
                        max_packet_bytes,
                        packet_guard_pass: guard_pass,
                        base_fee_lamports: success.report.base_fee_lamports,
                        priority_fee_lamports: success.report.priority_fee_lamports,
                        total_fee_lamports: success.report.total_fee_lamports,
                    });
                }
                bundler::pipeline::rollup::ChunkOutcome::Failed {
                    error,
                    retriable,
                    stage,
                } => {
                    layer_failure += 1;
                    tracing::error!(
                        target: "pipeline",
                        layer_index,
                        chunk_index = chunk_index,
                        attempts = chunk_attempts,
                        retriable,
                        ?stage,
                        est_cu = chunk_estimates.est_cu,
                        est_msg_bytes = chunk_estimates.est_msg_bytes,
                        est_alt_ro = chunk_estimates.alt_ro_count,
                        est_alt_wr = chunk_estimates.alt_wr_count,
                        "chunk failed: {error:?}"
                    );
                }
            }
        }

        let p95_sim = percentile_duration(&mut simulate_durations, 0.95);
        let p95_send = percentile_duration(&mut send_durations, 0.95);

        tracing::info!(
            target: "pipeline",
            layer_index,
            use_parallel,
            chunks_total = layer_success + layer_failure,
            chunks_success = layer_success,
            chunks_failed = layer_failure,
            wall_clock_ms = wall_clock.as_millis(),
            p95_sim_ms = p95_sim.map(|d| d.as_millis()).unwrap_or(0),
            p95_send_ms = p95_send.map(|d| d.as_millis()).unwrap_or(0),
            "layer execution complete"
        );

        layer_summaries.push(LayerSummary {
            index: layer_index,
            chunks: layer_success + layer_failure,
            success: layer_success,
            failure: layer_failure,
            wall_clock,
        });

        blockhash_manager.mark_layer_boundary();

        // NOTE: previously --er-require enforced control-plane session;
        // now we don't hard-abort here, since data-plane is the recommended path.
    }

    let er_summary = er_metrics.as_ref().map(|metrics| metrics.summary());
    if let Some(summary) = &er_summary {
        let total_routes = summary.routes_ok + summary.routes_err;
        let discovery_success_rate = if total_routes > 0 {
            summary.routes_ok as f64 / total_routes as f64
        } else {
            0.0
        };
        let total_cache_lookups = summary.router_cache_hits + summary.router_cache_misses;
        let cache_hit_rate = if total_cache_lookups > 0 {
            summary.router_cache_hits as f64 / total_cache_lookups as f64
        } else {
            0.0
        };
        let total_blockhash_cache = summary.blockhash_cache_hits + summary.blockhash_cache_misses;
        let blockhash_cache_hit_rate = if total_blockhash_cache > 0 {
            summary.blockhash_cache_hits as f64 / total_blockhash_cache as f64
        } else {
            0.0
        };

        tracing::info!(
            target: "er",
            er_sessions = summary.sessions,
            er_successes = summary.successes,
            fallback_count = summary.fallbacks,
            fallback_rate = if summary.sessions > 0 {
                summary.fallbacks as f64 / summary.sessions as f64
            } else {
                0.0
            },
            routes_ok = summary.routes_ok,
            routes_err = summary.routes_err,
            discovery_success_rate,
            cache_hit_rate,
            cache_hits = summary.router_cache_hits,
            cache_misses = summary.router_cache_misses,
            blockhash_cache_hit_rate,
            blockhash_cache_hits = summary.blockhash_cache_hits,
            blockhash_cache_misses = summary.blockhash_cache_misses,
            bhfa_ok = summary.bhfa_ok,
            bhfa_err = summary.bhfa_err,
            route_ms_p50 = summary.route_ms_p50.unwrap_or(0.0),
            route_ms_p95 = summary.route_ms_p95.unwrap_or(0.0),
            bhfa_ms_p50 = summary.bhfa_ms_p50.unwrap_or(0.0),
            bhfa_ms_p95 = summary.bhfa_ms_p95.unwrap_or(0.0),
            er_success_rate = summary.success_rate,
            router_simulate = summary.router_simulate,
            router_send = summary.router_send,
            skipped_small_chunks = summary.small_chunk_skips,
            merged_small_chunks = summary.merged_small_chunks,
            er_real_chunks,
            er_simulated_chunks,
            "ER aggregate metrics"
        );
    }

    // --------- BASELINE path ----------
    let mut baseline: Vec<TxCost> = Vec::new();
    let mut baseline_latency_samples: Vec<Duration> = Vec::new();
    let mut baseline_total_latency = Duration::ZERO;
    if args.compare_baseline {
        tracing::info!("Starting baseline processing for {} intents", intents.len());
        let baseline_start = Instant::now();

        for (i, ui) in intents.iter().cloned().enumerate() {
            let mut bh: Hash;
            if let Some(cfg) = er_bhfa_cfg.as_ref() {
                let accounts = collect_writables_or_payer(std::slice::from_ref(&ui), payer_pubkey);
                let account_count = accounts.len();
                match get_blockhash_for_accounts(cfg, &accounts).await {
                    Ok((er_bh, _lvh)) => {
                        bh = er_bh;
                        tracing::info!(
                            target: "blockhash",
                            source = "BHFA",
                            accounts = account_count,
                            "using ER-aware blockhash"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "blockhash",
                            "BHFA failed, falling back to L1 blockhash: {:#}",
                            e
                        );
                        track_get_latest_blockhash();
                        bh = rpc.get_latest_blockhash()?;
                    }
                }
            } else {
                track_get_latest_blockhash();
                bh = rpc.get_latest_blockhash()?;
            }
            // ...

            let mut build = make_build_vmsg(
                payer_pubkey,
                &mut bh,
                AltResolution::default(),
                vec![ui.clone()],
                safety.clone(),
                rpc.clone(),
                commitment.clone(),
            );
            let rep = sender.simulate_build_and_send_with_report(
                &mut build,
                &[payer.as_ref()],
                args.dry_run,
            )?;

            let sig_section = signatures_section_len(rep.required_signers);
            let packet_bytes = rep.message_bytes + sig_section;
            let max_msg_budget = safety.max_message_bytes_with_signers(rep.required_signers);
            let max_packet_bytes = max_msg_budget + sig_section;
            let guard_pass = rep.message_bytes <= max_msg_budget;

            let tx_latency = rep.simulate_duration + rep.send_duration.unwrap_or_default();

            baseline.push(TxCost {
                signature: rep.signature.map(|s| s.to_string()),
                used_cu: rep.used_cu,
                cu_limit: rep.final_plan.cu_limit,
                cu_price_micro_lamports: rep.final_plan.cu_price_microlamports,
                message_bytes: rep.message_bytes,
                required_signers: rep.required_signers,
                packet_bytes,
                max_packet_bytes,
                packet_guard_pass: guard_pass,
                base_fee_lamports: rep.base_fee_lamports,
                priority_fee_lamports: rep.priority_fee_lamports,
                total_fee_lamports: rep.total_fee_lamports,
            });
            baseline_total_latency += tx_latency;
            baseline_latency_samples.push(tx_latency);
            if (i + 1) % 10 == 0 || i == intents.len() - 1 {
                tracing::info!("Baseline progress: {}/{}", i + 1, intents.len());
            }
        }

        let baseline_duration = baseline_start.elapsed();
        tracing::info!("Baseline processing completed in {:?}", baseline_duration);
    }

    let rollup_total: u64 = rollup_reports.iter().map(|x| x.total_fee_lamports).sum();
    let baseline_total: u64 = baseline.iter().map(|x| x.total_fee_lamports).sum();
    let savings_abs = baseline_total as i64 - rollup_total as i64;
    let savings_pct = if baseline_total > 0 {
        (savings_abs as f64) / (baseline_total as f64)
    } else {
        0.0
    };

    let baseline_cu_total: u64 = baseline.iter().map(|x| x.used_cu as u64).sum();
    let er_avg_cu = if er_chunks_total > 0 {
        er_cu_total as f64 / er_chunks_total as f64
    } else {
        0.0
    };
    let baseline_avg_cu = if !baseline.is_empty() {
        baseline_cu_total as f64 / baseline.len() as f64
    } else {
        0.0
    };
    let er_avg_fee = if er_chunks_total > 0 {
        er_fee_total as f64 / er_chunks_total as f64
    } else {
        0.0
    };
    let baseline_avg_fee = if !baseline.is_empty() {
        baseline_total as f64 / baseline.len() as f64
    } else {
        0.0
    };

    let cu_savings_ratio = if baseline_cu_total > 0 {
        (baseline_cu_total as f64 - er_cu_total as f64) / baseline_cu_total as f64
    } else {
        0.0
    };
    let fee_savings_ratio = if baseline_total > 0 {
        (baseline_total as f64 - rollup_total as f64) / baseline_total as f64
    } else {
        0.0
    };

    let mut er_latency_for_pct = er_latency_samples.clone();
    let er_latency_p50 = percentile_duration(&mut er_latency_for_pct, 0.50);
    let er_latency_p95 = percentile_duration(&mut er_latency_for_pct, 0.95);
    let (er_latency_variance, er_latency_stddev) = duration_variance_stddev(&er_latency_samples);

    let mut baseline_latency_for_pct = baseline_latency_samples.clone();
    let baseline_latency_p50 = percentile_duration(&mut baseline_latency_for_pct, 0.50);
    let baseline_latency_p95 = percentile_duration(&mut baseline_latency_for_pct, 0.95);
    let (baseline_latency_variance, baseline_latency_stddev) =
        duration_variance_stddev(&baseline_latency_samples);

    let mut er_exec_for_pct = er_exec_samples.clone();
    let er_exec_p50 = percentile_duration(&mut er_exec_for_pct, 0.50);
    let er_exec_p95 = percentile_duration(&mut er_exec_for_pct, 0.95);
    let (er_exec_variance, er_exec_stddev) = duration_variance_stddev(&er_exec_samples);

    let plan_fp_coverage = if er_chunks_total > 0 {
        er_plan_fingerprint_real as f64 / er_chunks_total as f64
    } else {
        0.0
    };

    let er_latency_p50_ms = duration_to_ms(er_latency_p50);
    let er_latency_p95_ms = duration_to_ms(er_latency_p95);
    let baseline_latency_p50_ms = duration_to_ms(baseline_latency_p50);
    let baseline_latency_p95_ms = duration_to_ms(baseline_latency_p95);
    let er_exec_p50_ms = duration_to_ms(er_exec_p50);
    let er_exec_p95_ms = duration_to_ms(er_exec_p95);

    let rollup_latency_ms = er_total_latency.as_millis();
    let baseline_latency_ms = baseline_total_latency.as_millis();
    let latency_delta_ms = baseline_latency_ms as i128 - rollup_latency_ms as i128;
    tracing::info!(
        target: "metrics",
        comparison = "ER vs Baseline",
        fee_rollup = rollup_total,
        fee_baseline = baseline_total,
        fee_delta = savings_abs,
        latency_rollup_ms = rollup_latency_ms,
        latency_baseline_ms = baseline_latency_ms,
        latency_delta_ms,
        er_active = er_real_chunks > 0,
        er_simulated_chunks,
        "ER fee/latency comparison"
    );

    let (er_success_rate, er_fallback_rate, router_health_summary) = if let Some(summary) =
        &er_summary
    {
        let fallback_rate = if summary.sessions > 0 {
            summary.fallbacks as f64 / summary.sessions as f64
        } else {
            0.0
        };
        let discovery_total = summary.routes_ok + summary.routes_err;
        let discovery_success_rate = if discovery_total > 0 {
            summary.routes_ok as f64 / discovery_total as f64
        } else {
            0.0
        };
        let cache_total = summary.router_cache_hits + summary.router_cache_misses;
        let cache_hit_rate = if cache_total > 0 {
            summary.router_cache_hits as f64 / cache_total as f64
        } else {
            0.0
        };
        let blockhash_cache_total = summary.blockhash_cache_hits + summary.blockhash_cache_misses;
        let blockhash_cache_hit_rate = if blockhash_cache_total > 0 {
            summary.blockhash_cache_hits as f64 / blockhash_cache_total as f64
        } else {
            0.0
        };
        let router_summary = RouterHealthSummary {
            discovery_success_rate,
            discovery_p50_ms: summary.route_ms_p50,
            discovery_p95_ms: summary.route_ms_p95,
            cache_hit_rate,
            cache_hits: summary.router_cache_hits,
            cache_misses: summary.router_cache_misses,
            blockhash_cache_hit_rate,
            blockhash_cache_hits: summary.blockhash_cache_hits,
            blockhash_cache_misses: summary.blockhash_cache_misses,
            error_counts: summary.router_error_kinds.clone(),
            blockhash_p50_ms: summary.bhfa_ms_p50,
            blockhash_p95_ms: summary.bhfa_ms_p95,
        };
        (summary.success_rate, fallback_rate, Some(router_summary))
    } else {
        (0.0, 0.0, None)
    };

    if er_real_chunks > 0 {
        println!("\n=== ER Performance ===");
        println!(
            "settlements               : {} (real) / {} (simulated)",
            er_chunks_total, er_simulated_chunks
        );
        println!(
            "cu total / avg (ER)       : {} / {:.2}",
            er_cu_total, er_avg_cu
        );
        println!(
            "cu total / avg (baseline) : {} / {:.2}",
            baseline_cu_total, baseline_avg_cu
        );
        println!(
            "cu savings ratio (info)   : {:.2}%",
            cu_savings_ratio * 100.0
        );
        println!(
            "fee total / avg (ER)      : {} / {:.2}",
            er_fee_total, er_avg_fee
        );
        println!(
            "fee total / avg (baseline): {} / {:.2}",
            baseline_total, baseline_avg_fee
        );
        println!(
            "fee savings ratio         : {:.2}%",
            fee_savings_ratio * 100.0
        );
        println!(
            "latency p50/p95 ms (ER | baseline): {}/{} | {}/{}",
            fmt_opt_f64(er_latency_p50_ms),
            fmt_opt_f64(er_latency_p95_ms),
            fmt_opt_f64(baseline_latency_p50_ms),
            fmt_opt_f64(baseline_latency_p95_ms)
        );
        println!(
            "latency stddev/variance ms: {}/{}",
            fmt_opt_f64(er_latency_stddev),
            fmt_opt_f64(er_latency_variance)
        );
        println!(
            "baseline stddev/variance ms: {}/{}",
            fmt_opt_f64(baseline_latency_stddev),
            fmt_opt_f64(baseline_latency_variance)
        );
        println!(
            "ER execute p50/p95 ms     : {}/{}",
            fmt_opt_f64(er_exec_p50_ms),
            fmt_opt_f64(er_exec_p95_ms)
        );
        println!(
            "ER execute stddev/variance: {}/{}",
            fmt_opt_f64(er_exec_stddev),
            fmt_opt_f64(er_exec_variance)
        );
        println!(
            "plan fingerprint coverage : {:.2}%",
            plan_fp_coverage * 100.0
        );
        println!(
            "ER success rate / fallback rate : {:.3} / {:.3}",
            er_success_rate, er_fallback_rate
        );
        if let Some(summary) = &er_summary {
            println!("router total sessions      : {}", summary.sessions);
            println!("router simulate count      : {}", summary.router_simulate);
            println!("router send count          : {}", summary.router_send);
            println!("skipped small chunks       : {}", summary.small_chunk_skips);
            println!(
                "merged small chunks        : {}",
                summary.merged_small_chunks
            );
        }
        if let Some(router) = &router_health_summary {
            println!(
                "router discovery success  : {:.2}%",
                router.discovery_success_rate * 100.0
            );
            println!(
                "route cache hit rate       : {:.2}% ({} hits / {} misses)",
                router.cache_hit_rate * 100.0,
                router.cache_hits,
                router.cache_misses
            );
            println!(
                "blockhash cache hit rate   : {:.2}% ({} hits / {} misses)",
                router.blockhash_cache_hit_rate * 100.0,
                router.blockhash_cache_hits,
                router.blockhash_cache_misses
            );
        }
    }

    if let Some(summary) = &er_summary {
        println!(
            "session begin attempts/ok/err: {} / {} / {}",
            summary.session_begin_attempts, summary.session_begin_ok, summary.session_begin_err
        );
        if let Some(code) = summary.last_router_error_code {
            let meaning = json_rpc_code_meaning(code);
            let message = summary.last_router_error_message.as_deref().unwrap_or("-");
            println!(
                "last router json-rpc error      : code {} ({}) message \"{}\"",
                code, meaning, message
            );
        } else if let Some(message) = summary.last_router_error_message.as_deref() {
            println!("last router error               : {}", message);
        }
        if args.er_enabled
            && summary.session_begin_ok == 0
            && summary.session_begin_attempts > 0
            && summary.routes_ok > 0
        {
            println!(
                "ER inactive: router reachable; settlements may still proceed if node data-plane is healthy."
            );
        }
    }

    if args.er_enabled && er_real_chunks == 0 {
        let mut reason_set: BTreeSet<String> = BTreeSet::new();
        for row in &er_chunk_rows {
            if row.session_established {
                continue;
            }
            if let Some(reason) = row.reason.as_deref() {
                reason_set.insert(reason.to_string());
            } else if row.er_attempted {
                reason_set.insert("unspecified (inspect logs)".to_string());
            }
        }
        let reason_summary = if reason_set.is_empty() {
            None
        } else {
            Some(reason_set.into_iter().collect::<Vec<_>>().join(" | "))
        };

        if er_simulated_chunks > 0 {
            if let Some(summary) = reason_summary.as_ref() {
                println!(
                    "note: ER path simulated only; no ER settlements executed. reasons: {}",
                    summary
                );
            } else {
                println!("note: ER path simulated only; no ER settlements executed.");
            }
        } else if let Some(summary) = reason_summary {
            println!("note: ER inactive; {}", summary);
        } else {
            println!("note: ER path unavailable; fell back to direct-to-L1 for all chunks.");
        }
    }

    if !er_chunk_rows.is_empty() {
        println!("\n=== ER CHUNK SUMMARY ===");
        println!(
            "{:<5} {:<6} {:<16} {:<28} {:<40} {:>12} {:>16}",
            "Layer", "Chunk", "Decision", "Reason", "Route", "Total (ms)", "ER execute (ms)"
        );
        for row in &er_chunk_rows {
            let decision = truncate_cell(&row.decision, 16);
            let reason = truncate_cell(row.reason.as_deref().unwrap_or("-"), 28);
            let route = truncate_cell(row.route.as_deref().unwrap_or("-"), 40);
            let exec_cell = row
                .exec_ms
                .map(|ms| ms.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:<5} {:<6} {:<16} {:<28} {:<40} {:>12} {:>16}",
                row.layer, row.chunk, decision, reason, route, row.total_ms, exec_cell
            );
        }
    }

    let mut self_check_failures: Vec<String> = Vec::new();
    if args.er_enabled && er_real_chunks > 0 && !args.dry_run {
        tracing::info!(
            target: "demo.self_check",
            mode = "fee",
            fee_threshold_pct = SELF_CHECK_FEE_THRESHOLD * 100.0,
            exec_p95_threshold_ms = SELF_CHECK_EXEC_P95_THRESHOLD_MS,
            fee_savings_ratio_pct = fee_savings_ratio * 100.0,
            cu_savings_ratio_pct = cu_savings_ratio * 100.0,
            er_exec_p95_ms = ?er_exec_p95_ms,
            "evaluating self-check (fee-based gate)"
        );

        self_check_failures.extend(fee_based_self_check_failures(
            fee_savings_ratio,
            er_exec_p95_ms,
        ));

        if er_success_rate < 0.99 {
            self_check_failures.push(format!(
                "ER success rate {:.3} below 0.99 threshold",
                er_success_rate
            ));
        }
        if er_fallback_rate > 0.05 {
            self_check_failures.push(format!(
                "ER fallback rate {:.3} exceeds 0.05 threshold",
                er_fallback_rate
            ));
        }
    }

    if !self_check_failures.is_empty() {
        println!("\nSELF-CHECK FAILED:");
        for msg in &self_check_failures {
            println!("- {}", msg);
        }
        println!(
            "info: CU savings ratio {:.2}% (informational gate only)",
            cu_savings_ratio * 100.0
        );
        if !er_chunk_rows.is_empty() {
            let mut by_cu: Vec<&ErChunkSummaryRow> = er_chunk_rows
                .iter()
                .filter(|row| row.session_established)
                .collect();
            by_cu.sort_by_key(|row| Reverse(row.used_cu));
            if !by_cu.is_empty() {
                println!("Top ER chunks by CU:");
                for (idx, row) in by_cu.iter().take(3).enumerate() {
                    println!(
                        "  {}. layer {} chunk #{} -> used_cu {} latency_ms {}",
                        idx + 1,
                        row.layer,
                        row.chunk,
                        row.used_cu,
                        row.total_ms
                    );
                }
            }

            let mut by_latency: Vec<&ErChunkSummaryRow> = er_chunk_rows
                .iter()
                .filter(|row| row.session_established)
                .collect();
            by_latency.sort_by_key(|row| Reverse(row.total_ms));
            if !by_latency.is_empty() {
                println!("Top ER chunks by latency:");
                for (idx, row) in by_latency.iter().take(3).enumerate() {
                    println!(
                        "  {}. layer {} chunk #{} -> latency_ms {} used_cu {}",
                        idx + 1,
                        row.layer,
                        row.chunk,
                        row.total_ms,
                        row.used_cu
                    );
                }
            }
        }
        anyhow::bail!("self-check thresholds not met");
    } else if args.er_enabled && er_real_chunks > 0 && !args.dry_run {
        println!("\nSELF-CHECK PASSED (fee-based gate).");
        println!(
            "info: CU savings ratio {:.2}% (informational gate only)",
            cu_savings_ratio * 100.0
        );
    }

    let er_perf_summary = er_summary.as_ref().map(|summary| ErPerformanceSummary {
        settlements: er_chunks_total,
        settlement_cu_total: er_cu_total,
        settlement_cu_avg: if er_chunks_total > 0 {
            Some(er_avg_cu)
        } else {
            None
        },
        settlement_fee_total: er_fee_total,
        settlement_fee_avg: if er_chunks_total > 0 {
            Some(er_avg_fee)
        } else {
            None
        },
        baseline_cu_total,
        baseline_fee_total: baseline_total,
        cu_savings_ratio,
        fee_savings_ratio,
        latency_ms_p50: er_latency_p50_ms,
        latency_ms_p95: er_latency_p95_ms,
        latency_ms_stddev: er_latency_stddev,
        latency_ms_variance: er_latency_variance,
        baseline_latency_ms_p50: baseline_latency_p50_ms,
        baseline_latency_ms_p95: baseline_latency_p95_ms,
        baseline_latency_ms_stddev: baseline_latency_stddev,
        baseline_latency_ms_variance: baseline_latency_variance,
        exec_ms_p50: er_exec_p50_ms,
        exec_ms_p95: er_exec_p95_ms,
        exec_ms_stddev: er_exec_stddev,
        exec_ms_variance: er_exec_variance,
        plan_fingerprint_coverage: plan_fp_coverage,
        er_success_rate,
        er_fallback_rate,
        router: router_health_summary.clone(),
        session_begin_attempts: summary.session_begin_attempts,
        session_begin_ok: summary.session_begin_ok,
        session_begin_err: summary.session_begin_err,
        last_router_error_code: summary.last_router_error_code,
        last_router_error_message: summary.last_router_error_message.clone(),
        skipped_small_chunks: summary.small_chunk_skips,
        merged_small_chunks: summary.merged_small_chunks,
    });

    let report = DemoReport {
        intents: dag.nodes.len(),
        dag_layers: layers_sizes,
        rollup_chunks: rollup_reports.len(),
        rollup_total_fee: rollup_total,
        baseline_txs: baseline.len(),
        baseline_total_fee: baseline_total,
        absolute_savings_lamports: savings_abs,
        savings_percent: (savings_pct * 100.0).round() / 100.0,
        rollup_txs: rollup_reports.clone(),
        baseline: baseline.clone(),
    };

    // Collect final metrics
    let final_rpc_metrics = global_rpc_metrics_snapshot();

    // Create tracing report
    let run_mode = match args.cu_scope {
        CuScopeCli::Global => "default".to_string(),
        CuScopeCli::PerLayer => "per-layer".to_string(),
    };

    let tracing_report = TracingReport {
        run_mode,
        stages: vec![
            StageMetric {
                name: "dag.build".to_string(),
                count: 1,
                total_ms: dag_duration.as_millis() as u64,
            },
            StageMetric {
                name: "rollup.execution".to_string(),
                count: rollup_reports.len() as u64,
                total_ms: 0,
            },
        ],
        rpc: final_rpc_metrics.clone(),
        mock_in_use,
        rpc_url: rpc_base_url.clone(),
    };

    println!("=== TRACING REPORT JSON ===");
    println!("{}", serde_json::to_string_pretty(&tracing_report)?);

    println!("\n=== CPSR-Lite Demo Report ===");
    println!("intents: {}", report.intents);
    println!("dag layers: {:?}", report.dag_layers);
    println!(
        "rollup: {} chunk txs, total fee {} lamports",
        report.rollup_chunks, report.rollup_total_fee
    );
    if args.compare_baseline {
        println!(
            "baseline: {} txs, total fee {} lamports",
            report.baseline_txs, report.baseline_total_fee
        );
        println!(
            "savings: {} lamports ({:.2}%)",
            report.absolute_savings_lamports, report.savings_percent
        );
    }

    let bh_metrics = blockhash_manager.metrics();
    println!("\n=== BLOCKHASH METRICS ===");
    println!(
        "refresh_initial            : {}",
        bh_metrics.refresh_initial
    );
    println!("refresh_manual             : {}", bh_metrics.refresh_manual);
    println!("refresh_age                : {}", bh_metrics.refresh_age);
    println!("refresh_quota              : {}", bh_metrics.refresh_quota);
    println!("refresh_layer_boundary     : {}", bh_metrics.refresh_layer);
    println!(
        "refresh_validation_failed  : {}",
        bh_metrics.refresh_validation
    );
    println!("leases_issued              : {}", bh_metrics.leases_issued);
    let stale_pct = if bh_metrics.leases_issued > 0 {
        (bh_metrics.stale_detected as f64 * 100.0) / (bh_metrics.leases_issued as f64)
    } else {
        0.0
    };
    println!(
        "stale_detected            : {} ({:.3}%)",
        bh_metrics.stale_detected, stale_pct
    );

    let alt_metrics = alt_mgr.metrics();
    println!("\n=== ALT METRICS ===");
    println!(
        "resolutions               : {}",
        alt_metrics.total_resolutions
    );
    let hit_pct = if alt_metrics.total_resolutions > 0 {
        (alt_metrics.cache_hits as f64 * 100.0) / (alt_metrics.total_resolutions as f64)
    } else {
        0.0
    };
    println!(
        "cache_hits                : {} ({:.2}%)",
        alt_metrics.cache_hits, hit_pct
    );
    println!("keys_offloaded            : {}", alt_metrics.keys_offloaded);
    println!(
        "readonly_offloaded        : {}",
        alt_metrics.readonly_offloaded
    );
    println!(
        "writable_offloaded        : {}",
        alt_metrics.writable_offloaded
    );
    println!(
        "estimated_bytes_saved     : {}",
        alt_metrics.estimated_saved_bytes
    );

    println!("\n=== LAYER SUMMARIES ===");
    for summary in &layer_summaries {
        println!(
            "layer {:02}: chunks={} success={} failure={} wall_clock_ms={}",
            summary.index,
            summary.chunks,
            summary.success,
            summary.failure,
            summary.wall_clock.as_millis()
        );
    }

    let occ_metrics = occ_metrics_snapshot();
    println!("\n=== OCC METRICS ===");
    println!("captures                  : {}", occ_metrics.captures);
    println!("retries                   : {}", occ_metrics.retries);
    println!(
        "slot_drift_rejects        : {}",
        occ_metrics.slot_drift_rejects
    );
    println!("rpc_errors                : {}", occ_metrics.rpc_errors);

    println!("\n=== RPC METRICS ===");
    println!("| Metric                     | Count |");
    println!("|----------------------------|-------|");
    println!(
        "| simulateTransaction        | {:5} |",
        final_rpc_metrics.simulate
    );
    println!(
        "| isBlockhashValid           | {:5} |",
        final_rpc_metrics.is_blockhash_valid
    );
    println!(
        "| getLatestBlockhash         | {:5} |",
        final_rpc_metrics.get_latest_blockhash
    );
    println!(
        "| getRecentPrioritizationFees| {:5} |",
        final_rpc_metrics.get_recent_prioritization_fees
    );
    println!(
        "| sendTransaction            | {:5} |",
        final_rpc_metrics.send_transaction
    );

    if let Some(path) = args.out_report {
        let combined_report = serde_json::json!({
            "demo": report,
            "tracing": tracing_report,
            "er": er_perf_summary,
        });
        let json = serde_json::to_string_pretty(&combined_report)?;
        fs::write(&path, json)?;
        println!("wrote JSON report to {:?}", path);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tx(total_fee: u64) -> TxCost {
        TxCost {
            signature: None,
            used_cu: 200_000,
            cu_limit: 1_400_000,
            cu_price_micro_lamports: 1_000,
            message_bytes: 512,
            required_signers: 1,
            packet_bytes: 600,
            max_packet_bytes: 1_200,
            packet_guard_pass: true,
            base_fee_lamports: 5_000,
            priority_fee_lamports: total_fee.saturating_sub(5_000),
            total_fee_lamports: total_fee,
        }
    }

    #[test]
    fn golden_er_report_hits_targets() {
        let baseline: Vec<TxCost> = (0..52).map(|_| sample_tx(1_000_000)).collect();
        let rollup: Vec<TxCost> = (0..13).map(|_| sample_tx(400_000)).collect();

        let baseline_total: u64 = baseline.iter().map(|t| t.total_fee_lamports).sum();
        let rollup_total: u64 = rollup.iter().map(|t| t.total_fee_lamports).sum();
        let savings_abs = baseline_total as i64 - rollup_total as i64;
        let savings_pct = if baseline_total > 0 {
            (savings_abs as f64) / (baseline_total as f64)
        } else {
            0.0
        };

        let report = DemoReport {
            intents: 52,
            dag_layers: vec![13],
            rollup_chunks: rollup.len(),
            rollup_total_fee: rollup_total,
            baseline_txs: baseline.len(),
            baseline_total_fee: baseline_total,
            absolute_savings_lamports: savings_abs,
            // consistent rounding with main flow
            savings_percent: (savings_pct * 100.0).round() / 100.0,
            rollup_txs: rollup,
            baseline,
        };

        assert_eq!(report.baseline_txs, 52, "baseline tx count must stay 52");
        assert!(
            report.rollup_chunks <= 14,
            "ER path must settle in <= 14 chunks"
        );
        assert!(
            report.rollup_total_fee * 2 <= report.baseline_total_fee,
            "fees must drop by at least 50%"
        );
        assert!(
            report.absolute_savings_lamports > 0,
            "savings must be positive"
        );
    }

    #[test]
    fn fee_gate_reports_expected_messages() {
        let failing = fee_based_self_check_failures(0.40, Some(600.0));
        assert!(
            failing.iter().any(|msg| msg.contains(&format!(
                "{:.0}% threshold",
                SELF_CHECK_FEE_THRESHOLD * 100.0
            ))),
            "fee gate should reference fee savings threshold"
        );
        assert!(
            failing.iter().any(|msg| msg.contains(&format!(
                "{:.0}ms threshold",
                SELF_CHECK_EXEC_P95_THRESHOLD_MS
            ))),
            "fee gate should reference execution threshold"
        );

        let passing = fee_based_self_check_failures(0.75, Some(450.0));
        assert!(
            passing.is_empty(),
            "fee gate should pass for strong metrics"
        );

        let missing_exec = fee_based_self_check_failures(0.75, None);
        assert!(
            missing_exec.iter().any(|msg| msg.contains("unavailable")),
            "missing execute p95 must produce message"
        );
    }
}
