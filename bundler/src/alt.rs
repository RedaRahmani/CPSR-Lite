use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use blake3::Hasher as BlakeHasher;
use cpsr_types::UserIntent;
use solana_program::address_lookup_table::AddressLookupTableAccount;
use solana_sdk::pubkey::Pubkey;

use crate::chunking::AltPolicy;
// NEW: catalog-backed selection
use crate::alt::table_catalog::TableCatalog;

pub mod table_catalog;

#[derive(Clone, Debug, Default)]
pub struct AltUsageStats {
    pub keys_offloaded: usize,
    pub readonly_offloaded: usize,
    pub writable_offloaded: usize,
    pub estimated_saved_bytes: usize,
}

#[derive(Clone, Debug, Default)]
pub struct AltResolution {
    pub tables: Vec<AddressLookupTableAccount>,
    pub stats: AltUsageStats,
    pub cache_hit: bool,
}

#[derive(Clone, Debug, Default)]
pub struct AltMetrics {
    pub total_resolutions: u64,
    pub cache_hits: u64,
    pub keys_offloaded: u64,
    pub readonly_offloaded: u64,
    pub writable_offloaded: u64,
    pub estimated_saved_bytes: u64,
}

pub trait AltManager: Send + Sync {
    fn resolve_tables(&self, payer: Pubkey, intents: &[UserIntent]) -> AltResolution;
    fn metrics(&self) -> AltMetrics;
}

pub struct NoAltManager;

impl AltManager for NoAltManager {
    fn resolve_tables(&self, _payer: Pubkey, _intents: &[UserIntent]) -> AltResolution {
        AltResolution::default()
    }

    fn metrics(&self) -> AltMetrics {
        AltMetrics::default()
    }
}

pub struct CachingAltManager {
    policy: AltPolicy,
    ttl: Duration,
    cache: Mutex<HashMap<CacheKey, CacheEntry>>, // keyset -> resolution
    metrics: Mutex<AltMetrics>,
    // NEW: optional real LUT catalog
    catalog: Option<Arc<TableCatalog>>,
}

struct CacheEntry {
    resolution: AltResolution,
    expires_at: Instant,
}

#[derive(Clone, Eq)]
struct CacheKey(Vec<u8>);

impl PartialEq for CacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Hash for CacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

#[derive(Default)]
struct KeyUsage {
    pubkey: Pubkey,
    writable: bool,
    signer: bool,
    freq: u32,
}

impl CachingAltManager {
    pub fn new(policy: AltPolicy, ttl: Duration) -> Self {
        Self {
            policy,
            ttl,
            cache: Mutex::new(HashMap::new()),
            metrics: Mutex::new(AltMetrics::default()),
            catalog: None,
        }
    }

    /// Use an external catalog of **real** LUTs. If omitted, falls back to synthetic tables.
    pub fn with_catalog(mut self, catalog: Arc<TableCatalog>) -> Self {
        self.catalog = Some(catalog);
        self
    }

    fn build_key_usage(&self, payer: Pubkey, intents: &[UserIntent]) -> HashMap<Pubkey, KeyUsage> {
        let mut usage: HashMap<Pubkey, KeyUsage> = HashMap::new();

        for intent in intents {
            for account in &intent.ix.accounts {
                let entry = usage.entry(account.pubkey).or_insert_with(|| KeyUsage {
                    pubkey: account.pubkey,
                    ..Default::default()
                });
                entry.freq = entry.freq.saturating_add(1);
                entry.writable |= account.is_writable;
                entry.signer |= account.is_signer;
            }
        }

        usage.remove(&payer);
        usage
    }

    fn compute_offload_plan(&self, payer: Pubkey, intents: &[UserIntent]) -> AltResolution {
        if !self.policy.enabled {
            return AltResolution::default();
        }

        let usage = self.build_key_usage(payer, intents);
        if usage.is_empty() {
            return AltResolution::default();
        }

        let mut entries: Vec<KeyUsage> = usage.into_iter().map(|(_, v)| v).collect();

        // Keys that must remain static: programs + signers.
        let mut must_keep = Vec::new();
        entries.retain(|info| {
            if info.signer {
                must_keep.push(info.pubkey);
                false
            } else {
                true
            }
        });

        let mut reserved_slots = self.policy.reserve_message_keys;
        if reserved_slots <= must_keep.len() {
            // No room for ALTs; bail out.
            return AltResolution::default();
        }
        reserved_slots -= must_keep.len();

        if entries.len() <= reserved_slots {
            return AltResolution::default();
        }

        entries.sort_by(|a, b| {
            let cost_a = key_cost(a, self.policy.per_alt_index_byte as i64);
            let cost_b = key_cost(b, self.policy.per_alt_index_byte as i64);
            cost_b.cmp(&cost_a)
        });

        let keep_static = entries.split_off(reserved_slots);
        let mut offload = keep_static;

        if offload.is_empty() {
            return AltResolution::default();
        }

        if offload.len() > self.policy.max_alt_keys {
            offload.truncate(self.policy.max_alt_keys);
        }

        // === Real catalog path ===
        if let Some(catalog) = &self.catalog {
            // Only offload keys that exist in provided LUTs
            let desired: Vec<Pubkey> = offload.iter().map(|u| u.pubkey).collect();
            let (tables, resolvable) = catalog.tables_for(&desired);

            if tables.is_empty() || resolvable.is_empty() {
                return AltResolution::default();
            }

            // Compute stats over resolvable keys only
            let (ro, wr) = offload
                .iter()
                .filter(|u| resolvable.contains(&u.pubkey))
                .fold((0usize, 0usize), |(ro, wr), u| {
                    if u.writable {
                        (ro, wr + 1)
                    } else {
                        (ro + 1, wr)
                    }
                });

            let saved = estimate_saved_bytes(
                &self.policy,
                &offload
                    .into_iter()
                    .filter(|u| resolvable.contains(&u.pubkey))
                    .collect::<Vec<_>>(),
            );

            let resolution = AltResolution {
                tables,
                stats: AltUsageStats {
                    keys_offloaded: resolvable.len(),
                    readonly_offloaded: ro,
                    writable_offloaded: wr,
                    estimated_saved_bytes: saved,
                },
                cache_hit: false,
            };

            // Simple cache on keyset (of resolvable keys)
            let mut selected = resolvable.clone();
            selected.sort();
            let cache_key = CacheKey::from_pubkeys(&selected);

            let mut cache = self.cache.lock().expect("alt cache mutex");
            cache.insert(
                cache_key,
                CacheEntry {
                    resolution: resolution.clone(),
                    expires_at: Instant::now() + self.ttl,
                },
            );

            return resolution;
        }

        // === Synthetic fallback (previous behaviour) ===
        offload.sort_by_key(|k| k.pubkey);
        let cache_key = CacheKey::from(&offload);

        let mut cache = self.cache.lock().expect("alt cache mutex");
        if let Some(entry) = cache.get(&cache_key) {
            if entry.expires_at > Instant::now() {
                let mut cached = entry.resolution.clone();
                cached.cache_hit = true;
                return cached;
            } else {
                cache.remove(&cache_key);
            }
        }

        let addresses: Vec<Pubkey> = offload.iter().map(|u| u.pubkey).collect();
        let table_key = derive_table_key(&addresses);
        let table = AddressLookupTableAccount {
            key: table_key,
            addresses: addresses.clone(),
        };

        let stats = AltUsageStats {
            keys_offloaded: addresses.len(),
            readonly_offloaded: offload.iter().filter(|u| !u.writable).count(),
            writable_offloaded: offload.iter().filter(|u| u.writable).count(),
            estimated_saved_bytes: estimate_saved_bytes(&self.policy, &offload),
        };

        let resolution = AltResolution {
            tables: vec![table],
            stats,
            cache_hit: false,
        };

        cache.insert(
            cache_key,
            CacheEntry {
                resolution: resolution.clone(),
                expires_at: Instant::now() + self.ttl,
            },
        );

        resolution
    }
}

impl AltManager for CachingAltManager {
    fn resolve_tables(&self, payer: Pubkey, intents: &[UserIntent]) -> AltResolution {
        let resolution = self.compute_offload_plan(payer, intents);

        // Cache hit detection for the real-catalog path (key not based on KeyUsage struct)
        if resolution.tables.is_empty() && self.catalog.is_some() {
            // No change
        }

        let mut metrics = self.metrics.lock().expect("alt metrics mutex");
        metrics.total_resolutions += 1;
        if resolution.cache_hit {
            metrics.cache_hits += 1;
        }
        metrics.keys_offloaded += resolution.stats.keys_offloaded as u64;
        metrics.readonly_offloaded += resolution.stats.readonly_offloaded as u64;
        metrics.writable_offloaded += resolution.stats.writable_offloaded as u64;
        metrics.estimated_saved_bytes += resolution.stats.estimated_saved_bytes as u64;

        resolution
    }

    fn metrics(&self) -> AltMetrics {
        self.metrics.lock().expect("alt metrics mutex").clone()
    }
}

fn key_cost(usage: &KeyUsage, per_index_byte: i64) -> i64 {
    let static_cost = 32i64 * usage.freq as i64;
    let alt_cost = (per_index_byte as i64) * usage.freq as i64;
    static_cost - alt_cost + if usage.writable { 8 } else { 0 }
}

fn estimate_saved_bytes(policy: &AltPolicy, offload: &[KeyUsage]) -> usize {
    let static_bytes = 32 * offload.len();
    let index_bytes = (policy.per_alt_index_byte * offload.len()) as usize;
    let table_bytes = policy.per_table_fixed_bytes;
    static_bytes.saturating_sub(index_bytes + table_bytes)
}

fn derive_table_key(addresses: &[Pubkey]) -> Pubkey {
    let mut hasher = BlakeHasher::new();
    for pk in addresses {
        hasher.update(pk.as_ref());
    }
    let hash = hasher.finalize();
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&hash.as_bytes()[..32]);
    Pubkey::new_from_array(bytes)
}

impl CacheKey {
    fn from(entries: &[KeyUsage]) -> Self {
        let mut data = Vec::with_capacity(entries.len() * 33);
        for entry in entries {
            data.extend_from_slice(entry.pubkey.as_ref());
            data.push(entry.writable as u8);
        }
        CacheKey(data)
    }

    fn from_pubkeys(pks: &[Pubkey]) -> Self {
        let mut data = Vec::with_capacity(pks.len() * 32);
        for pk in pks {
            data.extend_from_slice(pk.as_ref());
        }
        CacheKey(data)
    }
}
