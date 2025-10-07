// bundler/examples/rollup_cli.rs
use std::{fs, path::PathBuf, str::FromStr, time::Duration, sync::Arc};

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

use bundler::{
    alt::{AltManager, AltResolution, NoAltManager},
    chunking::{AltPolicy, BasicOracle as ChunkOracle, TxBudget, chunk_layer_with},
    dag::Dag,
    occ::{AccountFetcher, FetchedAccount, OccConfig, capture_occ_with_retries},
    sender::{ReliableSender, SendReport, CuScope},
    serializer::{BuildTxCtx, build_v0_message},
    fee::{FeeOracle},
};
use cpsr_types::UserIntent;
use estimator::config::SafetyMargins;
use serde::Serialize;

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

    /// Dry-run: simulate only, do not send
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// Also run a baseline: one tx per intent, then compare costs
    #[arg(long, default_value_t = true)]
    compare_baseline: bool,

    /// Choose how the CU window is learned for P95: "global" or "per-layer"
    #[arg(long, value_enum, default_value_t = CuScopeCli::Global)]
    cu_scope: CuScopeCli,

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

/// Bridge our OCC trait to the RPC client.
struct RpcAccountFetcher { rpc: Arc<RpcClient>, commitment: CommitmentConfig }
impl AccountFetcher for RpcAccountFetcher {
    fn fetch_many(&self, keys: &[Pubkey]) -> Result<std::collections::HashMap<Pubkey, FetchedAccount>, bundler::occ::OccError> {
        use bundler::occ::OccError;
        let resp = self.rpc
            .get_multiple_accounts_with_commitment(keys, self.commitment.clone())
            .map_err(|e| OccError::Rpc(e.to_string()))?;
        let observed_slot = resp.context.slot;
        let mut out = std::collections::HashMap::with_capacity(keys.len());
        for (i, maybe_acc) in resp.value.into_iter().enumerate() {
            let key = keys[i];
            let acc = maybe_acc.ok_or(OccError::MissingAccount(key))?;
            out.insert(key, FetchedAccount {
                key, lamports: acc.lamports, owner: acc.owner, data: acc.data, slot: observed_slot,
            });
        }
        Ok(out)
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

// Helper to build a VersionedMessage for a set of intents
fn make_build_vmsg<'a>(
    payer_pk: solana_sdk::pubkey::Pubkey,
    bh: &'a mut solana_sdk::hash::Hash,
    alts: bundler::alt::AltResolution,
    chunk_intents: Vec<cpsr_types::UserIntent>,
    safety_cfg: estimator::config::SafetyMargins,
    rpc: std::sync::Arc<solana_client::rpc_client::RpcClient>,
    commitment: solana_sdk::commitment_config::CommitmentConfig,
) -> impl FnMut(bundler::fee::FeePlan) -> anyhow::Result<solana_sdk::message::VersionedMessage> + 'a {
    move |plan: bundler::fee::FeePlan| {
        // Refresh blockhash if needed
        let still = rpc.is_blockhash_valid(bh, commitment.clone())?;
        if !still {
            *bh = rpc.get_latest_blockhash()?;
        }
        let ctx = BuildTxCtx {
            payer: payer_pk,
            blockhash: *bh,
            fee: plan,
            alts: alts.clone(),
            safety: safety_cfg.clone(),
        };
        build_v0_message(&ctx, &chunk_intents)
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    let payer = read_keypair_file(&args.payer)
        .map_err(|e| anyhow!("reading keypair {:?}: {e}", args.payer))?;
    let payer_pubkey = payer.pubkey();

    let commitment = parse_commitment(&args.commitment);
    let rpc = Arc::new(RpcClient::new_with_timeout_and_commitment(
        args.rpc.clone(), Duration::from_secs(25), commitment.clone(),
    ));

    // --------- Intents ----------
    let intents: Vec<UserIntent> = if let Some(path) = args.intents.as_ref() {
        let data = fs::read_to_string(path).context("reading intents file")?;
        let parsed: Vec<UserIntent> = serde_json::from_str(&data).context("parsing intents JSON")?;
        parsed.into_iter().map(|ui| ui.normalized()).collect()

    } else if args.demo_mixed {
        // Mixed workload: independent memos + contended transfers (same dest)
        let to_str = args.demo_conflicting_to.as_ref()
            .ok_or_else(|| anyhow!("--demo-mixed also requires --demo-conflicting-to <PUBKEY>"))?;
        let dest = Pubkey::from_str(to_str)
            .map_err(|_| anyhow!("--demo-conflicting-to must be a valid pubkey"))?;

        let mut v = Vec::new();

        // (1) Independent memos → zero account metas → no conflicts
        for i in 0..args.mixed_memos {
            let ix = memo_ix(&format!("CPSR demo memo #{i}"));
            // higher priority so they appear early; still no conflicts
            v.push(UserIntent::new(payer_pubkey, ix, 9, None));
        }

        // (2) Contended transfers: all write same dest → chained by DAG
        for i in 0..args.mixed_contended {
            let ix = system_instruction::transfer(&payer_pubkey, &dest, args.demo_lamports);
            let prio: u8 = match i % 3 { 0 => 7, 1 => 5, _ => 3 };
            v.push(UserIntent::new(payer_pubkey, ix, prio, None));
        }

        v

    } else if let Some(to_str) = &args.demo_conflicting_to {
        // All transfers contend on one dest (good to show serialization)
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
    let alt_mgr: Box<dyn AltManager> = Box::new(NoAltManager);
    let account_fetcher = RpcAccountFetcher { rpc: rpc.clone(), commitment: commitment.clone() };
    let fee_oracle: Box<dyn FeeOracle> = Box::new(bundler::fee::BasicFeeOracle {
        max_cu_limit: args.max_cu_limit.unwrap_or(1_400_000),
        min_cu_price: args.cu_price.unwrap_or(100), // non-zero floor by default
        max_cu_price: 5_000,
    });
    let sender = ReliableSender::with_scope(rpc.clone(), fee_oracle, args.cu_scope.into());

    // --------- DAG & layers (for the report) ----------
    let dag = Dag::build(intents.clone());
    let layers = dag.layers().context("topo layers")?;
    let layers_sizes: Vec<usize> = layers.iter().map(|l| l.len()).collect();

    // Grab a recent blockhash up front
    let mut blockhash = rpc.get_latest_blockhash()?;

    let mut rollup_reports: Vec<TxCost> = Vec::new();

    // --------- ROLLUP path: chunk layer-by-layer ----------
    for layer in layers {
        // Reset CU stats per layer if requested.
        sender.start_scope();

        let groups = chunk_layer_with(&layer, &dag.nodes, &tx_budget, &alt_policy, &ChunkOracle)?;
        for node_ids in groups {
            let chunk_intents: Vec<UserIntent> =
                node_ids.iter().map(|&nid| dag.nodes[nid as usize].clone()).collect();

            // OCC capture
            let _snap = capture_occ_with_retries(&account_fetcher, &chunk_intents, &occ_cfg)?;

            let alts: AltResolution = alt_mgr.resolve_tables(chunk_intents.len());
            let mut build = make_build_vmsg(
                payer_pubkey,
                &mut blockhash,
                alts,
                chunk_intents,
                safety.clone(),
                rpc.clone(),
                commitment.clone(),
            );

            let rep: SendReport = sender.simulate_build_and_send_with_report(&mut build, &[&payer], args.dry_run)?;
            rollup_reports.push(TxCost {
                signature: rep.signature.map(|s| s.to_string()),
                used_cu: rep.used_cu,
                cu_limit: rep.final_plan.cu_limit,
                cu_price_micro_lamports: rep.final_plan.cu_price_microlamports,
                message_bytes: rep.message_bytes,
                required_signers: rep.required_signers,
                base_fee_lamports: rep.base_fee_lamports,
                priority_fee_lamports: rep.priority_fee_lamports,
                total_fee_lamports: rep.total_fee_lamports,
            });
        }
        blockhash = rpc.get_latest_blockhash()?;
    }

    // --------- BASELINE path: one tx per intent ----------
    let mut baseline: Vec<TxCost> = Vec::new();
    if args.compare_baseline {
        for ui in intents.iter().cloned() {
            let mut build = make_build_vmsg(
                payer_pubkey,
                &mut blockhash,
                AltResolution::default(),
                vec![ui.clone()], // one-intent baseline
                safety.clone(),
                rpc.clone(),
                commitment.clone(),
            );
            let rep = sender.simulate_build_and_send_with_report(&mut build, &[&payer], args.dry_run)?;
            baseline.push(TxCost {
                signature: rep.signature.map(|s| s.to_string()),
                used_cu: rep.used_cu,
                cu_limit: rep.final_plan.cu_limit,
                cu_price_micro_lamports: rep.final_plan.cu_price_microlamports,
                message_bytes: rep.message_bytes,
                required_signers: rep.required_signers,
                base_fee_lamports: rep.base_fee_lamports,
                priority_fee_lamports: rep.priority_fee_lamports,
                total_fee_lamports: rep.total_fee_lamports,
            });
        }
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
        rollup_txs: rollup_reports,
        baseline,
    };

    // Pretty print + optional JSON
    println!("=== CPSR-Lite Demo Report ===");
    println!("intents: {}", report.intents);
    println!("dag layers: {:?}", report.dag_layers);
    println!("rollup: {} chunk txs, total fee {} lamports", report.rollup_chunks, report.rollup_total_fee);
    if args.compare_baseline {
        println!("baseline: {} txs, total fee {} lamports", report.baseline_txs, report.baseline_total_fee);
        println!("savings: {} lamports ({:.2}%)", report.absolute_savings_lamports, report.savings_percent);
    }
    if let Some(path) = args.out_report {
        let json = serde_json::to_string_pretty(&report)?;
        fs::write(&path, json)?;
        println!("wrote JSON report to {:?}", path);
    }

    Ok(())
}
