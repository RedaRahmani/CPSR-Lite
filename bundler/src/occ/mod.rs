//! OCC (Optimistic Concurrency Control) capture for CPSR-Lite bundler.
//!
//! Responsibilities:
//! - Collect the unique set of accounts touched by a planned set of intents
//! - Fetch their live chain state via an abstract `AccountFetcher` (RPC-agnostic)
//! - Compute compact `AccountVersion` fingerprints (key, lamports, owner, data_hash64, slot)
//! - Enforce a maximum slot-drift window across all fetched accounts
//! - Retry with exponential backoff to handle transient RPC issues
//!
//! Design goals:
//! - Deterministic: same inputs => same set, same ordering of outputs (sorted by Pubkey)
//! - Pluggable: you can provide any RPC backend by implementing `AccountFetcher`
//! - Safe-by-default: conservative defaults for retries and slot drift
//!
//! Notes:
//! - We deliberately keep this module synchronous to avoid forcing an async runtime here.
//!   Implementors can block_on their own async RPC client or use a blocking RPC client.
//! - On-chain, you should verify fields directly rather than recomputing BLAKE3.
//!   This module computes `data_hash64` off-chain from raw account data.

use std::collections::{BTreeSet, HashMap};
use std::thread;
use std::time::Duration;

use cpsr_types::{AccountVersion};
use solana_program::pubkey::Pubkey;

/// A single fetched account snapshot from your RPC layer.
#[derive(Debug, Clone)]
pub struct FetchedAccount {
    pub key: Pubkey,
    pub lamports: u64,
    pub owner: Pubkey,
    pub data: Vec<u8>,
    /// Slot at which this account view is valid.
    pub slot: u64,
}

/// Abstract RPC fetcher interface the bundler depends on.
///
/// Implement this for your preferred Solana RPC client.
/// You may choose to:
/// - Fetch accounts one-by-one or in batches
/// - Return the 'observed slot' for each account
/// - Perform your own internal retries/timeouts
pub trait AccountFetcher {
    /// Fetch the given list of account pubkeys and return a map of Pubkey -> FetchedAccount.
    ///
    /// Requirements:
    /// - MUST return an entry for every requested key (or error)
    /// - SHOULD try to use a single root/commitment level for consistency (e.g., "confirmed")
    /// - MAY batch internally for efficiency
    fn fetch_many(&self, keys: &[Pubkey]) -> Result<HashMap<Pubkey, FetchedAccount>, OccError>;
}

/// Configuration knobs for OCC capture.
#[derive(Debug, Clone)]
pub struct OccConfig {
    /// Maximum allowed difference between the min and max slot across fetched accounts.
    /// If exceeded, we consider the snapshot too "drifty" and retry (or fail).
    pub max_slot_drift: u64,
    /// Maximum number of fetch attempts.
    pub max_retries: usize,
    /// Base backoff duration in milliseconds (exponential backoff).
    pub backoff_ms: u64,
    /// Whether to include read-only accounts in OCC (recommended true).
    /// If false, only writable accounts are fingerprinted.
    pub include_readonly: bool,
}

impl Default for OccConfig {
    fn default() -> Self {
        Self {
            max_slot_drift: 20,     // ~ a handful of slots
            max_retries: 4,         // 1 initial + 3 retries
            backoff_ms: 80,         // 80ms, then 160ms, 320ms, 640ms...
            include_readonly: true, // OCC on all touched accounts by default
        }
    }
}

/// Errors from OCC capture.
#[derive(Debug, thiserror::Error)]
pub enum OccError {
    #[error("rpc fetch error: {0}")]
    Rpc(String),
    #[error("missing account in fetch result: {0}")]
    MissingAccount(Pubkey),
    #[error("slot drift too large (min={min}, max={max}, drift={drift}, allowed={allowed})")]
    SlotDrift { min: u64, max: u64, drift: u64, allowed: u64 },
    #[error("exceeded max retries")]
    RetriesExhausted,
}

/// Compact 64-bit hash of account data (first 8 bytes of BLAKE3 in LE).
#[inline]
pub fn data_hash64(data: &[u8]) -> u64 {
    let h = blake3::hash(data);
    let bytes = h.as_bytes(); // &[u8; 32]
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(out)
}

/// The result of OCC capture for a planned chunk.
#[derive(Debug, Clone)]
pub struct OccSnapshot {
    /// Sorted (by Pubkey) list of account versions for deterministic ordering.
    pub versions: Vec<AccountVersion>,
    /// Minimum and maximum slot observed across all fetched accounts.
    pub min_slot: u64,
    pub max_slot: u64,
}

impl OccSnapshot {
    /// True if the observed slots fit within the configured drift budget.
    pub fn within_drift(&self, max_drift: u64) -> bool {
        (self.max_slot - self.min_slot) <= max_drift
    }
}

/// Build the set of unique accounts to fingerprint from a group of intents.
/// - If `include_readonly` is false, only accounts marked Writable are returned.
/// - Deterministic ordering (sorted by Pubkey) is enforced.
pub fn collect_target_accounts(
    intents: &[cpsr_types::UserIntent],
    include_readonly: bool,
) -> Vec<Pubkey> {
    let mut set: BTreeSet<Pubkey> = BTreeSet::new(); // deterministic order
    for intent in intents {
        for acc in &intent.accesses {
            let writable = matches!(acc.access, cpsr_types::intent::AccessKind::Writable);
            if writable || include_readonly {
                set.insert(acc.pubkey);
            }
        }
    }
    set.into_iter().collect()
}

/// Capture OCC versions for the given intents with retries and drift control.
///
/// Steps:
/// 1) Build the unique account set (respecting include_readonly)
/// 2) Fetch all accounts via `fetcher`
/// 3) Compute `AccountVersion`s and aggregate min/max slot
/// 4) If slot drift exceeds budget, retry with backoff (up to max_retries)
pub fn capture_occ_with_retries<F: AccountFetcher>(
    fetcher: &F,
    intents: &[cpsr_types::UserIntent],
    cfg: &OccConfig,
) -> Result<OccSnapshot, OccError> {
    let targets = collect_target_accounts(intents, cfg.include_readonly);
    if targets.is_empty() {
        return Ok(OccSnapshot { versions: Vec::new(), min_slot: 0, max_slot: 0 });
    }

    let mut attempt = 0usize;
    let mut backoff_ms = cfg.backoff_ms;
    loop {
        attempt += 1;
        match capture_once(fetcher, &targets) {
            Ok(snap) => {
                if snap.within_drift(cfg.max_slot_drift) {
                    return Ok(snap);
                }
                if attempt >= cfg.max_retries {
                    return Err(OccError::SlotDrift {
                        min: snap.min_slot,
                        max: snap.max_slot,
                        drift: snap.max_slot - snap.min_slot,
                        allowed: cfg.max_slot_drift,
                    });
                }
                // Backoff and retry to try to catch a tighter slot window
                thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = backoff_ms.saturating_mul(2);
            }
            Err(e) => {
                if attempt >= cfg.max_retries {
                    return Err(OccError::RetriesExhausted);
                }
                // Transient RPC error; backoff and retry
                thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = backoff_ms.saturating_mul(2);
                // Continue loop
            }
        }
    }
}

/// Single-shot OCC capture without retries.
fn capture_once<F: AccountFetcher>(
    fetcher: &F,
    targets: &[Pubkey],
) -> Result<OccSnapshot, OccError> {
    let map = fetcher.fetch_many(targets)?;

    // Build versions deterministically (sorted by key)
    let mut versions: Vec<AccountVersion> = Vec::with_capacity(targets.len());
    let mut min_slot = u64::MAX;
    let mut max_slot = 0u64;

    for &key in targets {
        let fa = map.get(&key).ok_or(OccError::MissingAccount(key))?;
        min_slot = min_slot.min(fa.slot);
        max_slot = max_slot.max(fa.slot);

        let dh = data_hash64(&fa.data);
        versions.push(AccountVersion {
            key: fa.key,
            lamports: fa.lamports,
            owner: fa.owner,
            data_hash64: dh,
            slot: fa.slot,
        });
    }

    Ok(OccSnapshot { versions, min_slot, max_slot })
}

// -------------------------------
// Unit tests with a mock fetcher
// -------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use solana_program::pubkey;

    struct MockFetcher {
        // (key -> (lamports, owner, data, slot))
        inner: HashMap<Pubkey, (u64, Pubkey, Vec<u8>, u64)>,
        // if set, returns Rpc error once to test retry path
        fail_once: bool,
    }

    impl MockFetcher {
        fn new() -> Self {
            Self { inner: HashMap::new(), fail_once: false }
        }
        fn insert(&mut self, key: Pubkey, lamports: u64, owner: Pubkey, data: Vec<u8>, slot: u64) {
            self.inner.insert(key, (lamports, owner, data, slot));
        }
    }

    impl AccountFetcher for MockFetcher {
        fn fetch_many(&self, keys: &[Pubkey]) -> Result<HashMap<Pubkey, FetchedAccount>, OccError> {
            if self.fail_once {
                // simulate transient RPC error
                return Err(OccError::Rpc("transient".into()));
            }
            let mut out = HashMap::with_capacity(keys.len());
            for k in keys {
                let (lamports, owner, data, slot) = self.inner.get(k)
                    .ok_or_else(|| OccError::MissingAccount(*k))?
                    .clone();
                out.insert(*k, FetchedAccount {
                    key: *k, lamports, owner, data, slot
                });
            }
            Ok(out)
        }
    }

    #[test]
    fn capture_happy_path() {
        let a = pubkey!("A11111111111111111111111111111111111111111111");
        let b = pubkey!("B11111111111111111111111111111111111111111111");
        let owner = pubkey!("Own11111111111111111111111111111111111111111");

        let mut mf = MockFetcher::new();
        mf.insert(a, 10, owner, vec![1,2,3], 100);
        mf.insert(b, 20, owner, vec![4,5],   102);

        let intents: Vec<cpsr_types::UserIntent> = Vec::new(); // we’ll pass targets manually
        let cfg = OccConfig::default();

        // Directly call capture_once for simplicity
        let targets = vec![a, b];
        let snap = capture_once(&mf, &targets).unwrap();
        assert_eq!(snap.min_slot, 100);
        assert_eq!(snap.max_slot, 102);
        assert_eq!(snap.versions.len(), 2);
        let dh_a = data_hash64(&[1,2,3]);
        assert_eq!(snap.versions[0].data_hash64, dh_a);
    }

    #[test]
    fn drift_retries_then_fail() {
        let a = pubkey!("A11111111111111111111111111111111111111111111");
        let b = pubkey!("B11111111111111111111111111111111111111111111");
        let owner = pubkey!("Own11111111111111111111111111111111111111111");

        let mut mf = MockFetcher::new();
        // Force large drift
        mf.insert(a, 10, owner, vec![], 100);
        mf.insert(b, 20, owner, vec![], 1000);

        let intents: Vec<cpsr_types::UserIntent> = Vec::new();
        let cfg = OccConfig {
            max_slot_drift: 10,
            max_retries: 2,
            backoff_ms: 1,
            include_readonly: true,
        };

        let targets = vec![a, b];
        // Build a fake intent list to exercise collect_target_accounts
        let _ = intents;

        let res = capture_occ_with_retries(&mf, &[], &cfg); // empty intents -> ok
        assert!(res.is_ok());

        // use manual targets: call capture_once then check drift path through wrapper:
        let res2 = capture_occ_with_retries(&mf, &fake_intents_for(&targets), &cfg);
        assert!(matches!(res2, Err(OccError::SlotDrift { .. })));
    }

    fn fake_intents_for(keys: &[Pubkey]) -> Vec<cpsr_types::UserIntent> {
        use cpsr_types::intent::{AccountAccess, AccessKind};
        use solana_program::instruction::{AccountMeta, Instruction};
        let dummy_prog = Pubkey::new_unique();
        let actor = Pubkey::new_unique();
        let mut out = Vec::new();
        for &k in keys {
            let meta = vec![AccountMeta::new(k, false)];
            let accesses = vec![AccountAccess { pubkey: k, access: AccessKind::ReadOnly }];
            out.push(cpsr_types::UserIntent {
                actor,
                ix: Instruction { program_id: dummy_prog, accounts: meta, data: vec![] },
                accesses,
                priority: 0,
                expires_at_slot: None,
            });
        }
        out
    }
}
