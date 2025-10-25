use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar::instructions;
use arcium_anchor::prelude::*;
use arcium_client::idl::arcium::types::CallbackAccount;
use encrypted_ixs::plan_chunk::{MAX_INTENTS, PLAN_OUTPUT_SCALARS, TOTAL_SCALARS};

const COMP_DEF_OFFSET_PLAN_CHUNK: u32 = comp_def_offset("plan_chunk");
const PLAN_RESULT_SEED: &[u8] = b"plan-result";

declare_id!("GvrPsn6Shsuwf7d5CYe5HSXc1kWKFay26g9TG7GoZ6Jz");

#[error_code]
pub enum FinalizeError {
    #[msg("Invalid finalize transaction")]
    InvalidFinalizeTx,
    #[msg("Invalid account")]
    InvalidAccount,
}

#[account]
pub struct SignerAccount {
    pub bump: u8,
}
impl SignerAccount {
    pub const LEN: usize = 8 + 1;
}

// Verify the callback was invoked by Arcium and is last in the tx.
fn validate_callback_ixs(instructions_sysvar: &AccountInfo, arcium_program: &Pubkey) -> Result<()> {
    const ARCIUM_FINALIZE_COMPUTATION_DISCRIMINATOR: [u8; 8] =
        [43, 29, 152, 92, 241, 179, 193, 210];
    const ARCIUM_CALLBACK_COMPUTATION_DISCRIMINATOR: [u8; 8] =
        [11, 224, 42, 236, 0, 154, 74, 163];

    let curr_ix_index = instructions::load_current_index_checked(instructions_sysvar)?;
    require!(curr_ix_index != 0, FinalizeError::InvalidFinalizeTx);

    let prev_ix =
        instructions::load_instruction_at_checked((curr_ix_index as usize) - 1, instructions_sysvar)?;
    require!(prev_ix.program_id == *arcium_program, FinalizeError::InvalidFinalizeTx);
    require!(
        prev_ix.data[0..8] == ARCIUM_FINALIZE_COMPUTATION_DISCRIMINATOR
            || prev_ix.data[0..8] == ARCIUM_CALLBACK_COMPUTATION_DISCRIMINATOR,
        FinalizeError::InvalidFinalizeTx,
    );

    // Ensure this is the last ix
    require!(
        instructions::load_instruction_at_checked((curr_ix_index as usize) + 1, instructions_sysvar)
            .is_err_and(|err| err == ProgramError::InvalidArgument),
        FinalizeError::InvalidFinalizeTx,
    );
    Ok(())
}

// Build Arcium Argument list from encrypted scalars
pub(crate) fn build_arguments(
    ciphertexts: &[[u8; 32]],
    client_pubkey: [u8; 32],
    nonce: u128,
) -> Result<Vec<Argument>> {
    let mut args = Vec::with_capacity(2 + TOTAL_SCALARS);
    args.push(Argument::ArcisPubkey(client_pubkey));
    args.push(Argument::PlaintextU128(nonce));

    let mut cursor = 0usize;
    for _ in 0..MAX_INTENTS {
        args.push(Argument::EncryptedU64(
            *ciphertexts.get(cursor).ok_or(ErrorCode::InvalidCiphertextLen)?,
        ));
        cursor += 1;

        for _ in 0..32 {
            args.push(Argument::EncryptedU8(
                *ciphertexts.get(cursor).ok_or(ErrorCode::InvalidCiphertextLen)?,
            ));
            cursor += 1;
        }

        args.push(Argument::EncryptedU32(
            *ciphertexts.get(cursor).ok_or(ErrorCode::InvalidCiphertextLen)?,
        ));
        cursor += 1;

        args.push(Argument::EncryptedU16(
            *ciphertexts.get(cursor).ok_or(ErrorCode::InvalidCiphertextLen)?,
        ));
        cursor += 1;

        args.push(Argument::EncryptedU16(
            *ciphertexts.get(cursor).ok_or(ErrorCode::InvalidCiphertextLen)?,
        ));
        cursor += 1;
    }

    args.push(Argument::EncryptedU16(
        *ciphertexts.get(cursor).ok_or(ErrorCode::InvalidCiphertextLen)?,
    ));
    cursor += 1;

    require_eq!(cursor, TOTAL_SCALARS, ErrorCode::InvalidCiphertextLen);
    Ok(args)
}

#[program]
pub mod confidential_planner {
    use super::*;

    pub fn init_plan_chunk_comp_def(ctx: Context<InitPlanChunkCompDef>) -> Result<()> {
        // finalize_during_callback = true, cu_amount = 0, no circuit override, no finalize auth override
        init_comp_def(ctx.accounts, true, 0, None, None)?;
        Ok(())
    }

    pub fn plan_chunk(
        ctx: Context<PlanChunk>,
        computation_offset: u64,
        ciphertexts: Vec<[u8; 32]>,
        client_pubkey: [u8; 32],
        nonce: u128,
    ) -> Result<()> {
        require_eq!(ciphertexts.len(), TOTAL_SCALARS, ErrorCode::InvalidCiphertextLen);

        // Seed & bump hygiene for Signer PDA
        ctx.accounts.sign_pda_account.bump = ctx.bumps.sign_pda_account;

        // Initialize plan result header the first time we see this offset
        if ctx.accounts.plan_result.offset == 0 {
            ctx.accounts.plan_result.offset = computation_offset;
            ctx.accounts.plan_result.bump = ctx.bumps.plan_result;
            ctx.accounts.plan_result.completed = false;
            ctx.accounts.plan_result.ciphertext_len = 0;
            ctx.accounts.plan_result.latest_nonce = [0u8; 16];
        }
        require_eq!(ctx.accounts.plan_result.offset, computation_offset, ErrorCode::OffsetMismatch);

        // Clear previous state
        ctx.accounts.plan_result.completed = false;
        ctx.accounts.plan_result.ciphertext_len = 0;
        ctx.accounts.plan_result.latest_nonce = [0u8; 16];

        // Encode inputs and queue computation
        let args = build_arguments(&ciphertexts, client_pubkey, nonce)?;
        let callback_accounts = [CallbackAccount {
            pubkey: ctx.accounts.plan_result.key(),
            is_writable: true,
        }];

        queue_computation(
            &*ctx.accounts, // accounts implementing QueueCompAccs
            computation_offset,
            args,
            None,
            vec![PlanChunkCallback::callback_ix(&callback_accounts)],
        )?;
        Ok(())
    }

    // Keep this frame small and separate — helps with Solana's 4KB/frame stack limit.
    // See: https://solana.stackexchange.com/q/8766
    #[inline(never)]
    #[arcium_callback(encrypted_ix = "plan_chunk")]
    pub fn plan_chunk_callback(
        ctx: Context<PlanChunkCallback>,
        output: ComputationOutputs<PlanChunkOutput>,
    ) -> Result<()> {
        // Security: ensure Arcium called us and we're last in the tx
        validate_callback_ixs(&ctx.accounts.instructions_sysvar, &ctx.accounts.arcium_program.key())?;

        let computed = match output {
            ComputationOutputs::Success(PlanChunkOutput { ref field_0 }) => field_0,
            ComputationOutputs::Failure => return Err(ErrorCode::AbortedComputation.into()),
        };

        require_eq!(
            computed.ciphertexts.len(),
            PLAN_OUTPUT_SCALARS,
            ErrorCode::InvalidOutputCiphertextLen
        );

        // Small, stack-friendly updates only
        // Extract nonce from computation output (little-endian u128 → [u8; 16])
        let mut nonce_bytes = [0u8; 16];
        nonce_bytes.copy_from_slice(&computed.nonce.to_le_bytes());

        let plan_result = &mut ctx.accounts.plan_result;
        plan_result.completed = true;
        plan_result.ciphertext_len = computed.ciphertexts.len() as u16;
        plan_result.latest_nonce = nonce_bytes;

        // Emit event with conditional payload based on feature flag
        #[cfg(feature = "full-event-logs")]
        {
            // Dev mode: emit full ciphertext payload for debugging
            // WARNING: ~4KB payload may be truncated in production logs
            emit!(PlanComputed {
                offset: plan_result.offset,
                nonce: nonce_bytes,
                ciphertexts: computed.ciphertexts.to_vec(),
            });
        }

        #[cfg(not(feature = "full-event-logs"))]
        {
            // Production mode: emit only hash to avoid log truncation
            use anchor_lang::solana_program::keccak;
            let mut hasher_input = Vec::new();
            for ct in computed.ciphertexts.iter() {
                hasher_input.extend_from_slice(ct);
            }
            let hash = keccak::hash(&hasher_input);

            emit!(PlanComputedHash {
                offset: plan_result.offset,
                nonce: nonce_bytes,
                ciphertext_hash: hash.to_bytes(),
            });
        }

        Ok(())
    }
}

#[queue_computation_accounts("plan_chunk", payer)]
#[derive(Accounts)]
#[instruction(computation_offset: u64)]
pub struct PlanChunk<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(address = derive_mxe_pda!())]
    pub mxe_account: Account<'info, MXEAccount>,

    #[account(mut, address = derive_mempool_pda!())]
    /// CHECK: validated by Arcium program
    pub mempool_account: UncheckedAccount<'info>,

    #[account(mut, address = derive_execpool_pda!())]
    /// CHECK: validated by Arcium program
    pub executing_pool: UncheckedAccount<'info>,

    #[account(mut, address = derive_comp_pda!(computation_offset))]
    /// CHECK: validated by Arcium program
    pub computation_account: UncheckedAccount<'info>,

    #[account(address = derive_comp_def_pda!(COMP_DEF_OFFSET_PLAN_CHUNK))]
    pub comp_def_account: Account<'info, ComputationDefinitionAccount>,

    #[account(mut, address = derive_cluster_pda!(mxe_account))]
    pub cluster_account: Account<'info, Cluster>,

    #[account(mut, address = ARCIUM_FEE_POOL_ACCOUNT_ADDRESS)]
    pub pool_account: Account<'info, FeePool>,

    #[account(address = ARCIUM_CLOCK_ACCOUNT_ADDRESS)]
    pub clock_account: Account<'info, ClockAccount>,

    #[account(
        mut,
        seeds = [PLAN_RESULT_SEED, &computation_offset.to_le_bytes()],
        bump,
    )]
    pub plan_result: Account<'info, PlanResultAccount>,

    // signer PDA must exist; create if missing
    #[account(
        init_if_needed,
        payer = payer,
        space = SignerAccount::LEN,
        seeds = [SIGN_PDA_SEED],
        bump,
        address = derive_sign_pda!(),
    )]
    pub sign_pda_account: Account<'info, SignerAccount>,

    pub system_program: Program<'info, System>,
    pub arcium_program: Program<'info, Arcium>,
}

#[callback_accounts("plan_chunk")]
#[derive(Accounts)]
pub struct PlanChunkCallback<'info> {
    #[account(
        mut,
        seeds = [PLAN_RESULT_SEED, &plan_result.offset.to_le_bytes()],
        bump = plan_result.bump,
    )]
    pub plan_result: Account<'info, PlanResultAccount>,

    pub arcium_program: Program<'info, Arcium>,

    #[account(address = derive_comp_def_pda!(COMP_DEF_OFFSET_PLAN_CHUNK))]
    pub comp_def_account: Account<'info, ComputationDefinitionAccount>,

    #[account(address = ::anchor_lang::solana_program::sysvar::instructions::ID)]
    /// CHECK: validated by constraint
    pub instructions_sysvar: AccountInfo<'info>,
}

#[init_computation_definition_accounts("plan_chunk", payer)]
#[derive(Accounts)]
pub struct InitPlanChunkCompDef<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(mut, address = derive_mxe_pda!())]
    pub mxe_account: Box<Account<'info, MXEAccount>>,

    #[account(mut)]
    /// CHECK: created by Arcium program
    pub comp_def_account: UncheckedAccount<'info>,

    pub arcium_program: Program<'info, Arcium>,
    pub system_program: Program<'info, System>,
}

#[account]
pub struct PlanResultAccount {
    pub offset: u64,
    pub bump: u8,
    pub completed: bool,
    pub ciphertext_len: u16,
    pub latest_nonce: [u8; 16],
}
impl PlanResultAccount {
    pub const LEN: usize = 8 + 8 + 1 + 1 + 2 + 16;
}

#[event]
pub struct PlanComputedHash {
    pub offset: u64,
    pub nonce: [u8; 16],
    pub ciphertext_hash: [u8; 32],
}

#[event]
pub struct PlanComputed {
    pub offset: u64,
    pub nonce: [u8; 16],
    pub ciphertexts: Vec<[u8; 32]>,
}

#[error_code]
pub enum ErrorCode {
    #[msg("Cluster PDA not initialized")]
    ClusterNotSet,
    #[msg("Cluster PDA does not match expected value")]
    WrongCluster,
    #[msg("The computation was aborted")]
    AbortedComputation,
    #[msg("Unexpected ciphertext count for planner input")]
    InvalidCiphertextLen,
    #[msg("Unexpected ciphertext count for planner output")]
    InvalidOutputCiphertextLen,
    #[msg("Stored offset does not match computation offset")]
    OffsetMismatch,
}
