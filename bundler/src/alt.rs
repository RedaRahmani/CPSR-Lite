use solana_program::address_lookup_table::AddressLookupTableAccount;
/// Which LUTs to include in message compilation.
#[derive(Clone, Debug, Default)]
pub struct AltResolution {
    pub tables: Vec<AddressLookupTableAccount>,
}

pub trait AltManager: Send + Sync {
    fn resolve_tables(&self, _intents_count: usize) -> AltResolution { AltResolution::default() }
}

/// Fallback manager: no LUTs (always works).
pub struct NoAltManager;
impl AltManager for NoAltManager {}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_alt_manager_empty() {
        let m = super::NoAltManager;
        let r = m.resolve_tables(3);
        assert!(r.tables.is_empty());
    }
}
