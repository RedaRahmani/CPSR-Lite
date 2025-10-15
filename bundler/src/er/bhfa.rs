use std::{str::FromStr, time::Duration};

use anyhow::{anyhow, Context, Result};
use cpsr_types::UserIntent;
use reqwest::Client;
use solana_sdk::{hash::Hash, pubkey::Pubkey};
use url::Url;

/// Configuration required to perform router-level BHFA probes.
#[derive(Clone, Debug)]
pub struct BhfaConfig {
    pub router_url: Url,
    pub router_api_key: Option<String>,
    pub http_timeout: Duration,
    pub connect_timeout: Duration,
}

impl BhfaConfig {
    pub fn new(
        router_url: Url,
        router_api_key: Option<String>,
        http_timeout: Duration,
        connect_timeout: Duration,
    ) -> Self {
        Self {
            router_url,
            router_api_key,
            http_timeout,
            connect_timeout,
        }
    }

    pub fn api_key(&self) -> Option<&str> {
        self.router_api_key.as_deref()
    }
}

/// Collect the writable accounts from the provided intents, falling back to the payer if needed.
pub fn collect_writables_or_payer(intents: &[UserIntent], payer: Pubkey) -> Vec<Pubkey> {
    let mut accounts: Vec<Pubkey> = intents
        .iter()
        .flat_map(|ui| {
            ui.ix
                .accounts
                .iter()
                .filter(|meta| meta.is_writable)
                .map(|meta| meta.pubkey)
        })
        .collect();

    if accounts.is_empty() {
        accounts.push(payer);
    }

    accounts
}

/// Fetch a blockhash from the Magic Router using `getBlockhashForAccounts`.
pub async fn get_blockhash_for_accounts(
    cfg: &BhfaConfig,
    accounts: &[Pubkey],
) -> Result<(Hash, u64)> {
    let client = Client::builder()
        .timeout(cfg.http_timeout)
        .connect_timeout(cfg.connect_timeout)
        .build()
        .context("building BHFA client")?;

    let mut req = client
        .post(cfg.router_url.clone())
        .header("Content-Type", "application/json");

    if let Some(key) = cfg.api_key() {
        if !key.is_empty() {
            req = req.header("x-api-key", key);
        }
    }

    let pubkeys: Vec<String> = accounts.iter().map(|pk| pk.to_string()).collect();
    let body = serde_json::json!({
        "jsonrpc":"2.0",
        "id":1,
        "method":"getBlockhashForAccounts",
        "params":[ pubkeys ]
    });

    let resp = req
        .json(&body)
        .send()
        .await
        .context("sending getBlockhashForAccounts request")?;

    let payload: serde_json::Value = resp
        .json()
        .await
        .context("decoding getBlockhashForAccounts response")?;

    let result = payload
        .get("result")
        .ok_or_else(|| anyhow!("BHFA: no result field in response"))?;
    let blockhash_str = result
        .get("blockhash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("BHFA: missing blockhash"))?;
    let lvh = result
        .get("lastValidBlockHeight")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("BHFA: missing lastValidBlockHeight"))?;

    let hash = Hash::from_str(blockhash_str).context("BHFA: invalid blockhash format")?;

    Ok((hash, lvh))
}
