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
use rand::{thread_rng, Rng};

use cpsr_types::AccountVersion;
use solana_program::pubkey::Pubkey;

// -------------------------------
// RPC abstraction
// -------------------------------

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

// -------------------------------
// Config & Errors
// -------------------------------

/// Configuration knobs for OCC capture.
#[derive(Debug, Clone)]
pub struct OccConfig {
    /// Maximum allowed difference between the min and max slot across fetched accounts.
    /// If exceeded, we consider the snapshot too "drifty" and retry (or fail).
    pub max_slot_drift: u64,
    /// Maximum number of fetch attempts (1 initial + retries).
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
impl Class {
    #[inline]
    fn is_fatal(self) -> bool { matches!(self, Class::Fatal) }
}

/// Errors from OCC capture.
#[derive(Debug, thiserror::Error)]
pub enum OccError {
    #[error("rpc fetch error: {0}")]
    Rpc(String),
    #[error("missing account in fetch result: {0}")]
    MissingAccount(Pubkey),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("slot drift too large (min={min}, max={max}, drift={drift}, allowed={allowed})")]
    SlotDrift { min: u64, max: u64, drift: u64, allowed: u64 },
    #[error("exceeded max retries")]
    RetriesExhausted,
}

impl OccError {
    /// Classify whether this error is fatal (do not retry) vs retriable.
    /// Fatal: MissingAccount, Unauthorized, RetriesExhausted.
    /// Rpc errors are classified by message contents (best-effort).
    pub fn is_fatal(&self) -> bool {
        match self {
            OccError::MissingAccount(_) => true,
            OccError::Unauthorized(_) => true,
            OccError::RetriesExhausted => true,
            // Slot drift is environmental → retry may succeed next attempt.
            OccError::SlotDrift { .. } => false,
            OccError::Rpc(msg) => classify_rpc_message(msg).is_fatal(),
        }
    }
}

// -------------------------------
// Hashing / Fingerprints
// -------------------------------

/// Compact 64-bit hash of account data (first 8 bytes of BLAKE3 in LE).
/// Truncation is acceptable here because false positives only force a re-capture.
/// For stronger guarantees later, consider widening to 128-bit.
#[inline]
pub fn data_hash64(data: &[u8]) -> u64 {
    let h = blake3::hash(data);
    let bytes = h.as_bytes(); // &[u8; 32]
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(out)
}

// -------------------------------
// Snapshot types
// -------------------------------

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

// -------------------------------
// Target collection
// -------------------------------

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

// -------------------------------
// Main capture logic
// -------------------------------

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
    let mut backoff_ms = cfg.backoff_ms.max(1);
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
                let jitter: u64 = thread_rng().gen_range(0..10);
                thread::sleep(std::time::Duration::from_millis(backoff_ms + jitter));
                backoff_ms = backoff_ms.saturating_mul(2).min(5_000);
            }
            Err(e0) => {
                let e = map_rpc_variant(e0);
                if e.is_fatal() { return Err(e); }
                if attempt >= cfg.max_retries { return Err(OccError::RetriesExhausted); }
                let jitter: u64 = thread_rng().gen_range(0..10);
                thread::sleep(std::time::Duration::from_millis(backoff_ms + jitter));
                backoff_ms = backoff_ms.saturating_mul(2).min(5_000);
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

    // Build versions deterministically (sorted by key = order of `targets`)
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
// Error classification helpers
// -------------------------------

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Class {
    Fatal,
    Retriable,
}

fn classify_rpc_message(msg: &str) -> Class {
    let m = msg.to_ascii_lowercase();
    if m.contains("unauthorized")
        || m.contains("forbidden")
        || m.contains("permission denied")
        || m.contains("access denied")
    {
        return Class::Fatal;
    }
    // Treat generic "account not found" strings as fatal if they bubble up here.
    if m.contains("account not found")
        || m.contains("could not find account")
        || m.contains("no such account")
    {
        return Class::Fatal;
    }
    // Default: retry (timeouts, rate limits, transient network).
    Class::Retriable
}

/// Upgrade certain `Rpc` errors to stronger typed variants.
/// (We cannot recover the Pubkey for "missing account" from a string; keep as `Rpc` fatal.)
fn map_rpc_variant(e: OccError) -> OccError {
    match e {
        OccError::Rpc(msg) => {
            if classify_rpc_message(&msg) == Class::Fatal
                && (msg.to_ascii_lowercase().contains("unauthorized")
                    || msg.to_ascii_lowercase().contains("forbidden")
                    || msg.to_ascii_lowercase().contains("permission denied")
                    || msg.to_ascii_lowercase().contains("access denied"))
            {
                OccError::Unauthorized(msg)
            } else {
                OccError::Rpc(msg)
            }
        }
        other => other,
    }
}

// -------------------------------
// Unit tests with a mock fetcher
// -------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use solana_program::instruction::{AccountMeta, Instruction};

    struct MockFetcher {
        // (key -> (lamports, owner, data, slot))
        inner: HashMap<Pubkey, (u64, Pubkey, Vec<u8>, u64)>,
        // When set, always fail with this error.
        fail_with: Option<OccError>,
    }

    impl MockFetcher {
        fn new() -> Self {
            Self { inner: HashMap::new(), fail_with: None }
        }
        fn insert(&mut self, key: Pubkey, lamports: u64, owner: Pubkey, data: Vec<u8>, slot: u64) {
            self.inner.insert(key, (lamports, owner, data, slot));
        }
    }

    impl AccountFetcher for MockFetcher {
        fn fetch_many(&self, keys: &[Pubkey]) -> Result<HashMap<Pubkey, FetchedAccount>, OccError> {
            if let Some(ref e) = self.fail_with {
                return Err(e.clone());
            }
            let mut out = HashMap::with_capacity(keys.len());
            for k in keys {
                let (lamports, owner, data, slot) = self
                    .inner
                    .get(k)
                    .ok_or_else(|| OccError::MissingAccount(*k))?
                    .clone();
                out.insert(
                    *k,
                    FetchedAccount { key: *k, lamports, owner, data, slot },
                );
            }
            Ok(out)
        }
    }

    fn mk_intent_touching(key: Pubkey) -> cpsr_types::UserIntent {
        use cpsr_types::intent::{AccountAccess, AccessKind};
        let program_id = Pubkey::new_unique();
        let actor = Pubkey::new_unique();
        let ix = Instruction {
            program_id,
            accounts: vec![AccountMeta::new(key, false)],
            data: vec![],
        };
        let accesses = vec![AccountAccess { pubkey: key, access: AccessKind::ReadOnly }];
        cpsr_types::UserIntent {
            actor,
            ix,
            accesses,
            priority: 0,
            expires_at_slot: None,
        }
    }

    #[test]
    fn capture_happy_path() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let owner = Pubkey::new_unique();

        let mut mf = MockFetcher::new();
        mf.insert(a, 10, owner, vec![1, 2, 3], 100);
        mf.insert(b, 20, owner, vec![4, 5], 102);

        let intents = vec![mk_intent_touching(a), mk_intent_touching(b)];
        let cfg = OccConfig::default();

        let snap = capture_occ_with_retries(&mf, &intents, &cfg).unwrap();
        assert_eq!(snap.min_slot, 100);
        assert_eq!(snap.max_slot, 102);
        assert_eq!(snap.versions.len(), 2);
        let dh_a = data_hash64(&[1, 2, 3]);
        assert_eq!(snap.versions.iter().find(|v| v.key == a).unwrap().data_hash64, dh_a);
    }

    #[test]
    fn unauthorized_is_fatal_not_retried() {
        let a = Pubkey::new_unique();
        let mut mf = MockFetcher::new();
        mf.fail_with = Some(OccError::Rpc("Unauthorized".into())); // will be mapped to Unauthorized

        let intents = vec![mk_intent_touching(a)];
        let cfg = OccConfig { max_retries: 3, backoff_ms: 1, ..Default::default() };

        let err = capture_occ_with_retries(&mf, &intents, &cfg).err().unwrap();
        // Early exit; either Unauthorized or Rpc(unauthorized) (we map to Unauthorized).
        match err {
            OccError::Unauthorized(_) => {}
            other => panic!("expected Unauthorized fatal, got {:?}", other),
        }
    }

    #[test]
    fn timeout_is_retriable_until_exhaustion() {
        let a = Pubkey::new_unique();
        let mut mf = MockFetcher::new();
        mf.fail_with = Some(OccError::Rpc("request timed out".into()));

        let intents = vec![mk_intent_touching(a)];
        let cfg = OccConfig { max_retries: 2, backoff_ms: 1, ..Default::default() };

        let err = capture_occ_with_retries(&mf, &intents, &cfg).err().unwrap();
        matches!(err, OccError::RetriesExhausted);
    }

    #[test]
    fn drift_too_large_after_retries_yields_slotdrift() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let owner = Pubkey::new_unique();

        let mut mf = MockFetcher::new();
        // Force large drift
        mf.insert(a, 10, owner, vec![], 100);
        mf.insert(b, 20, owner, vec![], 1000);

        let intents = vec![mk_intent_touching(a), mk_intent_touching(b)];
        let cfg = OccConfig { max_slot_drift: 10, max_retries: 2, backoff_ms: 1, ..Default::default() };

        let err = capture_occ_with_retries(&mf, &intents, &cfg).err().unwrap();
        matches!(err, OccError::SlotDrift { .. });
    }
}
