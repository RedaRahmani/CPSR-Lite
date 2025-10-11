use std::{collections::HashMap, sync::Arc};

use anyhow::{anyhow, Result};
use solana_client::rpc_client::RpcClient;
use solana_program::address_lookup_table::{
    program::ID as ALT_PROGRAM_ID,
    state::AddressLookupTable,
};
use solana_program::address_lookup_table::AddressLookupTableAccount;
use solana_sdk::pubkey::Pubkey;
use tracing::{info, warn};

/// Read-only catalog of **real** on-chain LUTs, offering fast reverse lookups.
/// We fetch raw accounts and decode them using the on-chain ALT state, so this
/// works across Solana client versions (no reliance on `get_address_lookup_table`).
pub struct TableCatalog {
    tables: Vec<AddressLookupTableAccount>,
    /// reverse index: key -> (table_pubkey, index)
    reverse: HashMap<Pubkey, (Pubkey, u8)>,
}

impl TableCatalog {
    /// Build from the RPC by fetching a list of LUT pubkeys. Any table that fails
    /// to fetch or decode will be skipped with a warning.
    pub fn from_rpc(rpc: Arc<RpcClient>, table_pubkeys: Vec<Pubkey>) -> Result<Self> {
        let mut tables = Vec::new();

        for tpk in table_pubkeys {
            match rpc.get_account(&tpk) {
                Ok(acc) => {
                    if acc.owner != ALT_PROGRAM_ID {
                        warn!(target: "alt.catalog", table = %tpk, "account not owned by ALT program; skipping");
                        continue;
                    }
                    // Decode on-chain ALT state
                    let mut data_slice: &[u8] = &acc.data;
                    match AddressLookupTable::deserialize(&mut data_slice) {
                        Ok(state) => {
                            let acct = AddressLookupTableAccount {
                                key: tpk,
                                // NOTE: `addresses` is a Cow<[Pubkey]>; convert to Vec
                                addresses: state.addresses.to_vec(),
                            };
                            if acct.addresses.is_empty() {
                                warn!(target: "alt.catalog", table = %tpk, "decoded LUT has 0 addresses; skipping");
                                continue;
                            }
                            tables.push(acct);
                        }
                        Err(e) => {
                            warn!(target: "alt.catalog", table = %tpk, "failed to decode LUT state: {e:?}");
                        }
                    }
                }
                Err(e) => {
                    warn!(target: "alt.catalog", table = %tpk, "error fetching account: {e:?}");
                }
            }
        }

        if tables.is_empty() {
            return Err(anyhow!("no LUTs fetched; provide at least one valid table"));
        }
        info!(target: "alt.catalog", tables = tables.len(), "built table catalog");
        Ok(Self::from_tables(tables))
    }

    /// Build directly from already-known tables (handy for tests).
    pub fn from_tables(tables: Vec<AddressLookupTableAccount>) -> Self {
        let mut reverse: HashMap<Pubkey, (Pubkey, u8)> = HashMap::new();
        for t in &tables {
            for (i, pk) in t.addresses.iter().enumerate() {
                if i >= u8::MAX as usize { break; }
                reverse.insert(*pk, (t.key, i as u8));
            }
        }
        Self { tables, reverse }
    }

    /// For a set of desired keys, return:
    ///  - the minimal set of LUT accounts that cover any of them
    ///  - and the subset of keys that are actually resolvable (present in those LUTs)
    pub fn tables_for(&self, desired: &[Pubkey]) -> (Vec<AddressLookupTableAccount>, Vec<Pubkey>) {
        use std::collections::BTreeSet;
        let mut used_tables: BTreeSet<Pubkey> = BTreeSet::new();
        let mut resolvable: Vec<Pubkey> = Vec::new();

        for k in desired {
            if let Some((table, _idx)) = self.reverse.get(k) {
                used_tables.insert(*table);
                resolvable.push(*k);
            }
        }
        if used_tables.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let tables: Vec<_> = self.tables
            .iter()
            .filter(|t| used_tables.contains(&t.key))
            .cloned()
            .collect();
        (tables, resolvable)
    }

    /// Does this key exist in any catalogued LUT?
    pub fn lookup(&self, key: &Pubkey) -> Option<(Pubkey, u8)> {
        self.reverse.get(key).copied()
    }

    /// Expose all known tables (by reference)
    pub fn all(&self) -> &[AddressLookupTableAccount] {
        &self.tables
    }
}
