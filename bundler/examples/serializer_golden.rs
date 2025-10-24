// bundler/examples/serializer_golden.rs
// Helper binary to generate golden message bytes from JSON test cases
// Usage: cargo run --example serializer_golden < path/to/golden.json

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use solana_sdk::{
    address_lookup_table_account::AddressLookupTableAccount,
    compute_budget,
    compute_budget::ComputeBudgetInstruction,
    hash::{hashv, Hash},
    instruction::{AccountMeta, Instruction},
    message::{v0::Message as MessageV0, VersionedMessage},
    pubkey::Pubkey,
    system_instruction,
};
use std::io::{self, Read};
use std::str::FromStr;

#[derive(Deserialize)]
struct GoldenTestCase {
    description: String,
    payer: String,
    #[serde(rename = "recentBlockhash")]
    recent_blockhash: String,
    #[serde(rename = "cuLimit")]
    cu_limit: u32,
    #[serde(rename = "cuPriceMicrolamports")]
    cu_price: u64,
    #[serde(default)]
    memo: Option<String>,
    #[serde(default)]
    transfers: Vec<Transfer>,
    #[serde(default)]
    alts: AltLookups,
}

#[derive(Deserialize)]
struct Transfer {
    from: String,
    to: String,
    lamports: u64,
}

#[derive(Deserialize, Default)]
struct AltLookups {
    #[serde(default)]
    lookups: Vec<AltLookup>,
}

#[derive(Deserialize)]
struct AltLookup {
    #[serde(rename = "tablePubkey")]
    table_pubkey: String,
    #[serde(rename = "writableIndexes")]
    writable_indexes: Vec<u8>,
    #[serde(rename = "readonlyIndexes")]
    readonly_indexes: Vec<u8>,
}

#[derive(Serialize)]
struct GoldenOutput {
    description: String,
    message_base64: String,
    message_bytes: usize,
}

/// Deterministically derive an address for a given ALT and index:
/// sha256(table_pubkey || index_byte)[0..32]
fn derive_alt_address(table_key: &Pubkey, index: u8) -> Pubkey {
    let h = hashv(&[table_key.as_ref(), &[index]]);
    Pubkey::new_from_array(h.to_bytes())
}

fn main() -> Result<()> {
    // Read JSON from stdin
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| anyhow!("Failed to read stdin: {}", e))?;

    let test_case: GoldenTestCase =
        serde_json::from_str(&input).map_err(|e| anyhow!("Failed to parse JSON: {}", e))?;

    // Parse payer and blockhash
    let payer =
        Pubkey::from_str(&test_case.payer).map_err(|e| anyhow!("Invalid payer pubkey: {}", e))?;
    let blockhash = Hash::from_str(&test_case.recent_blockhash)
        .map_err(|e| anyhow!("Invalid blockhash: {}", e))?;

    // 1) Compute budget instructions (prepended, limit -> price)
    let mut instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(test_case.cu_limit),
        ComputeBudgetInstruction::set_compute_unit_price(test_case.cu_price),
    ];

    // 2) Add transfer instructions
    for transfer in &test_case.transfers {
        let from = Pubkey::from_str(&transfer.from)
            .map_err(|e| anyhow!("Invalid from pubkey: {}", e))?;
        let to = Pubkey::from_str(&transfer.to)
            .map_err(|e| anyhow!("Invalid to pubkey: {}", e))?;
        instructions.push(system_instruction::transfer(&from, &to, transfer.lamports));
    }

    // 3) Optional memo (SPL Memo program id is stable on chain)
    if let Some(memo_text) = &test_case.memo {
        const MEMO_PROGRAM_ID: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";
        let memo_program = Pubkey::from_str(MEMO_PROGRAM_ID).expect("valid memo program id");
        instructions.push(Instruction {
            program_id: memo_program,
            accounts: vec![],
            data: memo_text.as_bytes().to_vec(),
        });
    }

    // 4) Build ALT accounts with deterministic addresses
    let mut alt_accounts: Vec<AddressLookupTableAccount> = Vec::new();
    let mut alt_anchor_metas: Vec<AccountMeta> = Vec::new();

    for lookup in &test_case.alts.lookups {
        let key = Pubkey::from_str(&lookup.table_pubkey).expect("invalid ALT key");

        let max_index = lookup
            .writable_indexes
            .iter()
            .chain(lookup.readonly_indexes.iter())
            .max()
            .copied()
            .unwrap_or(0);

        let mut addresses: Vec<Pubkey> = Vec::with_capacity((max_index as usize) + 1);
        for i in 0..=max_index {
            addresses.push(derive_alt_address(&key, i));
        }

        // Collect anchor metas so the lookups are actually used by the message
        for &i in &lookup.writable_indexes {
            let pk = addresses[i as usize];
            alt_anchor_metas.push(AccountMeta::new(pk, false)); // writable, non-signer
        }
        for &i in &lookup.readonly_indexes {
            let pk = addresses[i as usize];
            alt_anchor_metas.push(AccountMeta::new_readonly(pk, false)); // readonly, non-signer
        }

        // AddressLookupTableAccount structure with deterministic addresses
        alt_accounts.push(AddressLookupTableAccount { key, addresses });
    }

    // If any ALTs were provided, add a tiny "ALT anchor" instruction that
    // references exactly the indices we declared, guaranteeing the LUTs are used.
    if !alt_anchor_metas.is_empty() {
        let anchor_ix = Instruction {
            program_id: compute_budget::id(), // stable program id
            accounts: alt_anchor_metas,
            data: vec![], // no-op payload; used only to reference ALT keys
        };
        instructions.push(anchor_ix);
    }

    // 5) Compile v0 message
    let message = MessageV0::try_compile(&payer, &instructions, &alt_accounts, blockhash)
        .map_err(|e| anyhow!("Failed to compile v0 message: {}", e))?;

    let versioned_message = VersionedMessage::V0(message);

    // 6) Serialize
    let message_bytes = bincode::serialize(&versioned_message)
        .map_err(|e| anyhow!("Failed to serialize message: {}", e))?;

    let message_base64 = BASE64.encode(&message_bytes);

    // 7) Output JSON
    let output = GoldenOutput {
        description: test_case.description,
        message_base64,
        message_bytes: message_bytes.len(),
    };

    println!(
        "{}",
        serde_json::to_string(&output).map_err(|e| anyhow!("Failed to serialize output: {}", e))?
    );

    Ok(())
}
