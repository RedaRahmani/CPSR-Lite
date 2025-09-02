use anyhow::{anyhow, Result};
use bincode;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::Instruction,
    message::v0::Message as MessageV0,
    message::VersionedMessage,
    pubkey::Pubkey,
};
use cpsr_types::UserIntent;
use estimator::config::SafetyMargins;
use crate::fee::FeePlan;
use crate::alt::AltResolution;

/// Context for one transaction build (unsigned message only).
#[derive(Clone, Debug)]
pub struct BuildTxCtx {
    pub payer: Pubkey,
    pub blockhash: Hash,
    pub fee: FeePlan,        // cu limit/price
    pub alts: AltResolution, // chosen LUT accounts
    pub safety: SafetyMargins,
}

/// Build a v0 *message* (not a signed transaction). Caller will sign later.
pub fn build_v0_message(ctx: &BuildTxCtx, intents: &[UserIntent]) -> Result<VersionedMessage> {
    if intents.is_empty() { return Err(anyhow!("no intents")); }

    // 1) Prepend compute-budget ixs.
    let mut ixs: Vec<Instruction> = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(ctx.fee.cu_limit as u32),
        ComputeBudgetInstruction::set_compute_unit_price(ctx.fee.cu_price_microlamports),
    ];
    ixs.extend(intents.iter().map(|it| it.ix.clone()));

    // 2) Compile v0 with optional LUTs.
    let msg = MessageV0::try_compile(&ctx.payer, &ixs, &ctx.alts.tables, ctx.blockhash)?;
    let vmsg = VersionedMessage::V0(msg.clone());

    // 3) Enforce packet-size budget: signatures (64B each) + message ≤ budget.
    let message_bytes = bincode::serialize(&vmsg)?.len();
    let required_signers = msg.header.num_required_signatures as usize;
    let max_msg = ctx.safety.max_message_bytes_with_signers(required_signers);
    if message_bytes > max_msg {
        return Err(anyhow!(
            "message {}B exceeds budget {}B (reserved for {} signers)",
            message_bytes, max_msg, required_signers
        ));
    }

    Ok(vmsg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::instruction::AccountMeta;

    fn mk_intent() -> UserIntent {
        let program = Pubkey::new_unique();
        let acc = Pubkey::new_unique();
        let ix = Instruction {
            program_id: program,
            accounts: vec![AccountMeta::new(acc, false)],
            data: vec![1,2,3],
        };
        UserIntent::new(Pubkey::new_unique(), ix, 0, None)
    }

    #[test]
    fn builds_v0_and_prepends_compute_budget_ixs() {
        let payer = Pubkey::new_unique();
        let ctx = BuildTxCtx {
            payer,
            blockhash: Hash::new_unique(),
            fee: FeePlan { cu_limit: 100_000, cu_price_microlamports: 0 },
            alts: AltResolution::default(),
            safety: SafetyMargins::default(),
        };
        let vmsg = build_v0_message(&ctx, &[mk_intent()]).expect("build ok");
        match vmsg {
            VersionedMessage::V0(m) => {
                assert!(m.instructions.len() >= 3); // 2 compute-budget + 1 user ix
            }
            _ => panic!("expected v0"),
        }
    }

    #[test]
    fn fails_when_message_budget_too_small() {
        let payer = Pubkey::new_unique();
        let tiny = SafetyMargins { max_msg_bytes: 16, bytes_slack: 0, ..SafetyMargins::default() };
        let ctx = BuildTxCtx {
            payer,
            blockhash: Hash::new_unique(),
            fee: FeePlan { cu_limit: 100_000, cu_price_microlamports: 0 },
            alts: AltResolution::default(),
            safety: tiny,
        };
        let err = build_v0_message(&ctx, &[mk_intent()]).err().expect("should error");
        assert!(format!("{}", err).contains("exceeds budget"));
    }
}
