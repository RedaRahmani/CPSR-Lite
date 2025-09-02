//! RPC-backed AccountFetcher for OCC capture.
//!
//! Uses `getMultipleAccounts` and stamps each result with the response slot.
//! Commitment is configurable; defaults to `processed`.
//!
//! NOTE: `getMultipleAccounts` returns a single slot for the whole batch; we
//! attach that slot to every returned account (good enough for OCC).
use std::collections::HashMap;

use solana_client::{
    rpc_client::RpcClient,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
};

use crate::occ::{AccountFetcher, FetchedAccount, OccError};

#[derive(Clone)]
pub struct RpcAccountFetcher {
    rpc: RpcClient,
    commitment: CommitmentConfig,
}

impl RpcAccountFetcher {
    pub fn new(rpc: RpcClient, commitment: CommitmentConfig) -> Self {
        Self { rpc, commitment }
    }
}

impl AccountFetcher for RpcAccountFetcher {
    fn fetch_many(&self, keys: &[Pubkey]) -> Result<HashMap<Pubkey, FetchedAccount>, OccError> {
        use solana_client::rpc_response::Response as RpcResponse;

        let RpcResponse { context, value } =
            self.rpc.get_multiple_accounts_with_commitment(keys, self.commitment)
                .map_err(|e| OccError::Rpc(format!("get_multiple_accounts: {e}")))?;

        let slot = context.slot;
        let mut out = HashMap::with_capacity(keys.len());
        for (i, acc_opt) in value.into_iter().enumerate() {
            let key = keys[i];
            let acc = acc_opt.ok_or(OccError::MissingAccount(key))?;
            out.insert(
                key,
                FetchedAccount {
                    key,
                    lamports: acc.lamports,
                    owner: acc.owner,
                    data: acc.data,
                    slot,
                },
            );
        }
        Ok(out)
    }
}
