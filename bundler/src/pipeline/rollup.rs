use anyhow::{Result, anyhow};
use cpsr_types::{
    bundle::{Bundle, Chunk},
    UserIntent,
};
use estimator::config::SafetyMargins;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::Keypair,
};

use crate::{
    dag::Dag,
    chunking::{chunk_layer_with, TxBudget, AltPolicy, BasicOracle},
    occ::{OccConfig, capture_occ_with_retries, AccountFetcher as _},
    alt::{AltManager, AltResolution},
    fee::{FeeOracle, FeePlan},
    serializer::{BuildTxCtx, build_v0_tx},
    sender::ReliableSender,
};

/// External deps the pipeline needs.
pub struct RollupDeps {
    pub rpc: RpcClient,
    pub fee_oracle: Box<dyn FeeOracle>,
    pub alt_mgr: Box<dyn AltManager>,
    pub account_fetcher: Box<dyn crate::occ::AccountFetcher>,
}

/// Static config for planning/safety.
pub struct RollupCfg {
    pub safety: SafetyMargins,
    pub occ: OccConfig,
    pub tx_budget: TxBudget,
    pub alt_policy: AltPolicy,
    pub payer: Pubkey,
}

pub struct RollupResult {
    pub bundle: Bundle,
    pub signatures: Vec<solana_sdk::signature::Signature>,
}

pub fn execute_rollup(
    intents: Vec<UserIntent>,
    deps: RollupDeps,
    cfg: RollupCfg,
    payer_keypair: &Keypair,
) -> Result<RollupResult> {
    if intents.is_empty() { return Err(anyhow!("no intents supplied")); }

    // 1) DAG → layers (conflict-safe)
    let dag = Dag::build(intents);
    let layers = dag.layers()?;

    let mut chunks_out: Vec<Chunk> = Vec::new();
    let mut sigs = Vec::new();

    let sender = ReliableSender { rpc: deps.rpc.clone(), fee_oracle: deps.fee_oracle };

    // get a recent blockhash up front; may refresh between chunks if needed
    let mut blockhash = deps.rpc.get_latest_blockhash()?;

    for layer in layers {
        // 2) chunk the layer under budgets
        let chunk_node_groups = chunk_layer_with(&layer, &dag.nodes, &cfg.tx_budget, &cfg.alt_policy, &BasicOracle)?;
        for node_ids in chunk_node_groups {
            // gather intents for this chunk
            let chunk_intents: Vec<UserIntent> =
                node_ids.iter().map(|&nid| dag.nodes[nid as usize].clone()).collect();

            // 3) OCC capture (w/ retries + drift control)
            let snap = capture_occ_with_retries(deps.account_fetcher.as_ref(), &chunk_intents, &cfg.occ)?;

            // 4) construct chunk (compute Merkle over intent digests)
            let mut chunk = Chunk {
                intents: chunk_intents.clone(),
                occ_versions: snap.versions.clone(),
                merkle_root: [0u8; 32],
                session_authority: None,
                nonce: 0,
                last_valid_block_height: 0,
                est_cu: 0,
                est_msg_bytes: 0,
            };
            chunk.recompute_merkle_root();

            // 5) resolve LUTs (safe fallback if unavailable) :contentReference[oaicite:9]{index=9}
            let alts: AltResolution = deps.alt_mgr.resolve_tables(chunk.intents.len());

            // 6) build + simulate + price + sign + send
            let payer = cfg.payer;
            let safety = cfg.safety.clone();

            let build = |plan: FeePlan| -> Result<solana_sdk::transaction::VersionedTransaction> {
                // refresh blockhash if going stale; cheap guard
                let still_valid = deps.rpc.is_blockhash_valid(&blockhash, CommitmentConfig::processed())?;
                if !still_valid {
                    blockhash = deps.rpc.get_latest_blockhash()?;
                }
                let ctx = BuildTxCtx {
                    payer,
                    blockhash,
                    fee: plan,
                    alts: alts.clone(),
                    safety: safety.clone(),
                };
                build_v0_tx(&ctx, &chunk_intents)
            };

            let sig = sender.simulate_and_send(build, &[payer_keypair])?;
            sigs.push(sig);

            // store the built chunk for bundle commitment
            chunks_out.push(chunk);
        }

        // allow blockhash refresh between layers
        blockhash = deps.rpc.get_latest_blockhash()?;
    }

    // 7) bundle commitment over chunk IDs
    let mut bundle = Bundle {
        chunks: chunks_out,
        bundle_root: [0u8; 32],
        created_at_slot: deps.rpc.get_slot()?,
        max_age_slots: 300,
    };
    bundle.recompute_root();

    Ok(RollupResult { bundle, signatures: sigs })
}
