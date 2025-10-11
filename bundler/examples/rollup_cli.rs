// bundler/examples/rollup_cli.rs
use std::{fs, path::PathBuf, str::FromStr, time::{Duration, Instant}, sync::Arc};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    signature::read_keypair_file,
    signer::Signer,
    system_instruction,
    pubkey::Pubkey,
};
use bundler::pipeline::blockhash::BlockhashProvider;
use bundler::{
    alt::{AltManager, AltResolution, NoAltManager, CachingAltManager},
    alt::table_catalog::TableCatalog,
    chunking::{AltPolicy, BasicOracle as ChunkOracle, TxBudget, chunk_layer_with},
    dag::Dag,
    fee::FeeOracle,
    // ⬇️ Drop FetchedAccount import; we only need OccConfig + metrics here
    occ::{OccConfig, occ_metrics_snapshot},
    // ⬇️ Import the REAL RPC fetcher implementation
    occ::rpc_fetcher::RpcAccountFetcher,
    pipeline::{
        blockhash::{BlockhashManager, BlockhashPolicy},
        rollup::{
            send_layer_parallel, ChunkEstimates, ChunkOutcome, ChunkPlan, LayerResult,
            ParallelLayerConfig, ParallelLayerDeps,
        },
    },
    sender::{
        ReliableSender, CuScope, global_rpc_metrics_snapshot, track_get_latest_blockhash,
    },
    serializer::signatures_section_len,
};

use cpsr_types::UserIntent;
use estimator::config::SafetyMargins;
use serde::Serialize;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FeeOracleCli { Basic, Recent }

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

    /// Provide on-chain LUT table pubkeys (comma-separated). If set, the ALT manager
    /// will only offload keys found in these tables (real selection).
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
}

#[derive(Subcommand, Debug)]
enum Cmd {}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CuScopeCli { Global, PerLayer }
impl From<CuScopeCli> for CuScope {
    fn from(v: CuScopeCli) -> Self {
        match v {
            CuScopeCli::Global => CuScope::Global,
            CuScopeCli::PerLayer => CuScope::PerScope,
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

#[derive(Clone, Debug)]
struct LayerSummary {
    index: usize,
    chunks: usize,
    success: usize,
    failure: usize,
    wall_clock: Duration,
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
    let memo_pid = Pubkey::from_str("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr").expect("memo program id");
    Instruction { program_id: memo_pid, accounts: vec![], data: text.as_bytes().to_vec() }
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

// (make_build_vmsg helper unchanged)
fn make_build_vmsg<'a>(
    payer_pk: solana_sdk::pubkey::Pubkey,
    bh: &'a mut solana_sdk::hash::Hash,
    alts: bundler::alt::AltResolution,
    chunk_intents: Vec<cpsr_types::UserIntent>,
    safety_cfg: estimator::config::SafetyMargins,
    _rpc: std::sync::Arc<solana_client::rpc_client::RpcClient>,
    _commitment: solana_sdk::commitment_config::CommitmentConfig,
) -> impl FnMut(bundler::fee::FeePlan) -> anyhow::Result<solana_sdk::message::VersionedMessage> + 'a {
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
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        )
        .init();

    let args = Args::parse();

    let payer = Arc::new(
        read_keypair_file(&args.payer)
            .map_err(|e| anyhow!("reading keypair {:?}: {e}", args.payer))?
    );
    let payer_pubkey = payer.pubkey();

    let commitment = parse_commitment(&args.commitment);
    let rpc = Arc::new(RpcClient::new_with_timeout_and_commitment(
        args.rpc.clone(), Duration::from_secs(25), commitment.clone(),
    ));

    // Mock check
    let mock_in_use = args.rpc.contains("mock") || std::env::var("SOLANA_RPC_MOCK").is_ok();
    tracing::info!(rpc_url = %args.rpc, mock_in_use = mock_in_use, "RPC client initialized");

    // --------- Intents ----------
    let intents: Vec<UserIntent> = if let Some(path) = args.intents.as_ref() {
        let data = fs::read_to_string(path).context("reading intents file")?;
        let parsed: Vec<UserIntent> = serde_json::from_str(&data).context("parsing intents JSON")?;
        parsed.into_iter().map(|ui| ui.normalized()).collect()
    } else if args.demo_mixed {
        let to_str = args.demo_conflicting_to.as_ref()
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
            let prio: u8 = match i % 3 { 0 => 7, 1 => 5, _ => 3 };
            v.push(UserIntent::new(payer_pubkey, ix, prio, None));
        }
        v
    } else if let Some(to_str) = &args.demo_conflicting_to {
        let dest = Pubkey::from_str(to_str)
            .map_err(|_| anyhow!("--demo-conflicting-to must be a valid pubkey"))?;
        (0..args.demo_count).map(|_| {
            let ix = system_instruction::transfer(&payer_pubkey, &dest, args.demo_lamports);
            UserIntent::new(payer_pubkey, ix, 0, None)
        }).collect()
    } else {
        return Err(anyhow!("Provide either --intents PATH, --demo-mixed (with --demo-conflicting-to), or --demo-conflicting-to PUBKEY"));
    };
    if intents.is_empty() { return Err(anyhow!("No intents to run")); }

    // --------- Static config ----------
    let safety = SafetyMargins::default();
    let tx_budget = TxBudget::default();
    let alt_policy = AltPolicy { enabled: args.enable_alt, ..AltPolicy::default() };
    let occ_cfg = OccConfig::default();

    // --------- Deps ----------
    // Optional: build a real LUT catalog if --alt-tables is provided
    let catalog = if let Some(list) = args.alt_tables.as_deref() {
        let table_pks: Vec<Pubkey> = list
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| Pubkey::from_str(s))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| anyhow!("invalid --alt-tables list"))?;
        if table_pks.is_empty() { None } else {
            Some(Arc::new(TableCatalog::from_rpc(rpc.clone(), table_pks)?))
        }
    } else { None };

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
    tracing::info!(
        target: "blockhash",
        max_age_ms = max_age_ms,
        refresh_every = refresh_every,
        pre_check = !args.fast_send,
        "Blockhash policy configured"
    );

    // Choose fee oracle
    let fee_oracle: Box<dyn FeeOracle> = match args.fee_oracle {
        FeeOracleCli::Basic => {
            Box::new(bundler::fee::BasicFeeOracle {
                max_cu_limit: args.max_cu_limit.unwrap_or(1_400_000),
                min_cu_price: args.min_cu_price.or(args.cu_price).unwrap_or(100),
                max_cu_price: args.max_cu_price.unwrap_or(5_000),
            })
        }
        FeeOracleCli::Recent => {
            use bundler::fee_oracles::recent::RecentFeesOracle;

            let probes: Vec<Pubkey> = args.probe_accounts
                .as_deref()
                .unwrap_or("")
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| Pubkey::from_str(s.trim()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| anyhow!("invalid --probe-accounts list"))?;

            let mut o = RecentFeesOracle::new(rpc.clone(), probes);
            if let Some(v) = args.max_cu_limit { o.max_cu_limit = v; }
            if let Some(v) = args.min_cu_price.or(args.cu_price) { o.min_cu_price = v; }
            if let Some(v) = args.max_cu_price { o.max_cu_price = v; }
            if let Some(v) = args.fee_pctile { o.p_primary = v; }
            if let Some(v) = args.fee_alpha { o.ema_alpha = v; }
            if let Some(v) = args.fee_hysteresis_bps { o.hysteresis_bps = v; }
            Box::new(o)
        }
    };

    // Build sender and apply fast-send if requested
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
        blockhash_provider: blockhash_manager.clone(),
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

    // --------- ROLLUP path ----------
    tracing::info!("Starting rollup processing with {} layers", layers.len());
    for (layer_index, layer) in layers.iter().enumerate() {
        let layer_span = tracing::info_span!("chunking.layer", layer_index = layer_index, layer_size = layer.len());
        let _guard = layer_span.enter();

        tracing::info!("Processing layer {} with {} intents", layer_index, layer.len());
        sender.start_scope();

        let chunking_start = Instant::now();
        let planned_chunks = chunk_layer_with(layer, &dag.nodes, &tx_budget, &alt_policy, &ChunkOracle)?;
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
            });
        }

        let mut cfg = parallel_cfg_template.clone();
        if chunk_plans.len() <= 1 {
            cfg.max_concurrency = 1;
        }
        let use_parallel = cfg.max_concurrency > 1 && chunk_plans.len() > 1;

        let LayerResult { layer_index: _, wall_clock, chunks } =
            send_layer_parallel(layer_index, chunk_plans, parallel_deps.clone(), cfg).await;

        let mut layer_success = 0usize;
        let mut layer_failure = 0usize;
        let mut simulate_durations: Vec<Duration> = Vec::new();
        let mut send_durations: Vec<Duration> = Vec::new();

        for chunk in chunks {
            match chunk.outcome {
                ChunkOutcome::Success(success) => {
                    layer_success += 1;
                    simulate_durations.push(success.report.simulate_duration);
                    if let Some(send_dur) = success.report.send_duration {
                        send_durations.push(send_dur);
                    }

                    let sig_section = signatures_section_len(success.report.required_signers);
                    let packet_bytes = success.report.message_bytes + sig_section;
                    let max_msg_budget =
                        safety.max_message_bytes_with_signers(success.report.required_signers);
                    let max_packet_bytes = max_msg_budget + sig_section;
                    let guard_pass = success.report.message_bytes <= max_msg_budget;

                    tracing::info!(
                        target: "pipeline",
                        layer_index,
                        chunk_index = chunk.index,
                        attempts = chunk.attempts,
                        total_ms = chunk.timings.total.as_millis(),
                        simulate_ms = success.report.simulate_duration.as_millis(),
                        send_ms = success
                            .report
                            .send_duration
                            .map(|d| d.as_millis())
                            .unwrap_or(0),
                        est_cu = chunk.estimates.est_cu,
                        est_msg_bytes = chunk.estimates.est_msg_bytes,
                        alt_keys = success.alt_resolution.stats.keys_offloaded,
                        alt_saved_bytes = success.alt_resolution.stats.estimated_saved_bytes,
                        last_valid_block_height = success.lease.last_valid_block_height,
                        "chunk completed"
                    );

                    if let Some(sig) = &success.report.signature {
                        tracing::info!(
                            target: "pipeline",
                            layer_index,
                            chunk_index = chunk.index,
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
                ChunkOutcome::Failed { error, retriable, stage } => {
                    layer_failure += 1;
                    tracing::error!(
                        target: "pipeline",
                        layer_index,
                        chunk_index = chunk.index,
                        attempts = chunk.attempts,
                        retriable,
                        ?stage,
                        est_cu = chunk.estimates.est_cu,
                        est_msg_bytes = chunk.estimates.est_msg_bytes,
                        est_alt_ro = chunk.estimates.alt_ro_count,
                        est_alt_wr = chunk.estimates.alt_wr_count,
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
    }

    // --------- BASELINE path ----------
    let mut baseline: Vec<TxCost> = Vec::new();
    if args.compare_baseline {
        tracing::info!("Starting baseline processing for {} intents", intents.len());
        let baseline_start = Instant::now();
        
        for (i, ui) in intents.iter().cloned().enumerate() {
            track_get_latest_blockhash();
            let mut bh = rpc.get_latest_blockhash()?;

            let mut build = make_build_vmsg(
                payer_pubkey,
                &mut bh,
                AltResolution::default(),
                vec![ui.clone()],
                safety.clone(),
                rpc.clone(),
                commitment.clone(),
            );
            let rep = sender.simulate_build_and_send_with_report(&mut build, &[payer.as_ref()], args.dry_run)?;

            let sig_section = signatures_section_len(rep.required_signers);
            let packet_bytes = rep.message_bytes + sig_section;
            let max_msg_budget = safety.max_message_bytes_with_signers(rep.required_signers);
            let max_packet_bytes = max_msg_budget + sig_section;
            let guard_pass = rep.message_bytes <= max_msg_budget;

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
        (savings_abs as f64) / (baseline_total as f64) * 100.0
    } else { 0.0 };

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
        rpc_url: args.rpc.clone(),
    };
    
    println!("=== TRACING REPORT JSON ===");
    println!("{}", serde_json::to_string_pretty(&tracing_report)?);
    
    println!("\n=== CPSR-Lite Demo Report ===");
    println!("intents: {}", report.intents);
    println!("dag layers: {:?}", report.dag_layers);
    println!("rollup: {} chunk txs, total fee {} lamports", report.rollup_chunks, report.rollup_total_fee);
    if args.compare_baseline {
        println!("baseline: {} txs, total fee {} lamports", report.baseline_txs, report.baseline_total_fee);
        println!("savings: {} lamports ({:.2}%)", report.absolute_savings_lamports, report.savings_percent);
    }
    
    let bh_metrics = blockhash_manager.metrics();
    println!("\n=== BLOCKHASH METRICS ===");
    println!("refresh_initial            : {}", bh_metrics.refresh_initial);
    println!("refresh_manual             : {}", bh_metrics.refresh_manual);
    println!("refresh_age                : {}", bh_metrics.refresh_age);
    println!("refresh_quota              : {}", bh_metrics.refresh_quota);
    println!("refresh_layer_boundary     : {}", bh_metrics.refresh_layer);
    println!("refresh_validation_failed  : {}", bh_metrics.refresh_validation);
    println!("leases_issued              : {}", bh_metrics.leases_issued);
    let stale_pct = if bh_metrics.leases_issued > 0 {
        (bh_metrics.stale_detected as f64 * 100.0) / (bh_metrics.leases_issued as f64)
    } else {
        0.0
    };
    println!(
        "stale_detected            : {} ({:.3}%)",
        bh_metrics.stale_detected,
        stale_pct
    );

        let alt_metrics = alt_mgr.metrics();
    println!("\n=== ALT METRICS ===");
    println!("resolutions               : {}", alt_metrics.total_resolutions);
    let hit_pct = if alt_metrics.total_resolutions > 0 {
        (alt_metrics.cache_hits as f64 * 100.0) / (alt_metrics.total_resolutions as f64)
    } else {
        0.0
    };
    println!(
        "cache_hits                : {} ({:.2}%)",
        alt_metrics.cache_hits,
        hit_pct
    );
    println!("keys_offloaded            : {}", alt_metrics.keys_offloaded);
    println!("readonly_offloaded        : {}", alt_metrics.readonly_offloaded);
    println!("writable_offloaded        : {}", alt_metrics.writable_offloaded);
    println!("estimated_bytes_saved     : {}", alt_metrics.estimated_saved_bytes);


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
    println!("slot_drift_rejects        : {}", occ_metrics.slot_drift_rejects);
    println!("rpc_errors                : {}", occ_metrics.rpc_errors);

    println!("\n=== RPC METRICS ===");
    println!("| Metric                     | Count |");
    println!("|----------------------------|-------|");
    println!("| simulateTransaction        | {:5} |", final_rpc_metrics.simulate);
    println!("| isBlockhashValid           | {:5} |", final_rpc_metrics.is_blockhash_valid);
    println!("| getLatestBlockhash         | {:5} |", final_rpc_metrics.get_latest_blockhash);
    println!("| getRecentPrioritizationFees| {:5} |", final_rpc_metrics.get_recent_prioritization_fees);
    println!("| sendTransaction            | {:5} |", final_rpc_metrics.send_transaction);
    
    if let Some(path) = args.out_report {
        let combined_report = serde_json::json!({
            "demo": report,
            "tracing": tracing_report
        });
        let json = serde_json::to_string_pretty(&combined_report)?;
        fs::write(&path, json)?;
        println!("wrote JSON report to {:?}", path);
    }

    Ok(())
}
