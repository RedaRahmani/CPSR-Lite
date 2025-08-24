use anyhow::Context;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction as CBI,
    hash::Hash,
    instruction::Instruction,
    message::v0 as v0,
    message::VersionedMessage,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
    address_lookup_table::AddressLookupTableAccount,
    pubkey::Pubkey,
};
use crate::runtime::chunker::ChunkPlan;
use cpsr_types::UserIntent;

pub struct BuildOpts<'a> {
    pub signers: &'a [&'a Keypair],        // payer first
    pub recent_blockhash: Hash,
    pub cu_limit: u32,                     // ComputeBudget: limit
    pub cu_price_micro_lamports: u64,      // ComputeBudget: priority fee (μ-lamports per CU)
    pub alts: Vec<AddressLookupTableAccount>,
    pub prefix_ixs: Vec<Instruction>,
    pub suffix_ixs: Vec<Instruction>,
}

pub fn tx_from_plan(
    plan: &ChunkPlan,
    intents: &[UserIntent],
    opts: &BuildOpts,
) -> anyhow::Result<VersionedTransaction> {
    anyhow::ensure!(!opts.signers.is_empty(), "no signers");
    let payer: Pubkey = opts.signers[0].pubkey();

    let mut ixs = Vec::with_capacity(plan.instrs + 2 + opts.prefix_ixs.len() + opts.suffix_ixs.len());
    ixs.extend(opts.prefix_ixs.iter().cloned());
    ixs.push(CBI::set_compute_unit_limit(opts.cu_limit).into());
    ixs.push(CBI::set_compute_unit_price(opts.cu_price_micro_lamports).into());
    for nid in &plan.nodes {
        ixs.push(intents[*nid as usize].ix.clone());
    }
    ixs.extend(opts.suffix_ixs.iter().cloned());

    let v0msg = v0::Message::try_compile(&payer, &ixs, &opts.alts, opts.recent_blockhash)
        .context("compile v0 message")?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(v0msg), opts.signers)
        .context("sign versioned tx")?;
    Ok(tx)
}

pub fn wire_len(tx: &VersionedTransaction) -> anyhow::Result<usize> {
    Ok(bincode::serialize(tx)?.len())
}
