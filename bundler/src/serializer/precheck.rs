use anyhow::{anyhow, Result};
use solana_program::address_lookup_table::AddressLookupTableAccount;

use crate::alt::AltResolution;

/// Light, defensive checks so we fail fast with a clear error **before**
/// we call into Solana's encoder. This is intentionally simple.
pub fn validate_alt_resolution(alts: &AltResolution) -> Result<()> {
    for (i, t) in alts.tables.iter().enumerate() {
        if t.addresses.is_empty() {
            return Err(anyhow!("ALT table #{i} ({}) has no addresses", t.key));
        }
        // Basic consistency: no duplicate addresses within a table (rare but easy to guard)
        // (O(n log n) with a transient set; tiny tables so fine.)
        let mut set = std::collections::BTreeSet::new();
        for pk in &t.addresses {
            if !set.insert(*pk) {
                return Err(anyhow!(
                    "ALT table {} contains duplicate address {}",
                    t.key,
                    pk
                ));
            }
        }
    }
    Ok(())
}

/// In a future version we could provide a pruning pass that removes tables or keys
/// that can't be satisfied at build-time. Keeping the API here so callers can wire
/// it if/when needed.
#[allow(dead_code)]
pub fn prune_empty_tables(
    mut tables: Vec<AddressLookupTableAccount>,
) -> Vec<AddressLookupTableAccount> {
    tables.retain(|t| !t.addresses.is_empty());
    tables
}
