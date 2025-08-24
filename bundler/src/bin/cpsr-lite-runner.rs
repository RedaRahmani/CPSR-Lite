use std::{thread, time::Duration, collections::{HashMap, BTreeSet}};
use bundler::runtime::{
    config::RuntimeConfig,
    sources::{IntentSource, VecSource},
    planner, occ_guard::capture_layer_snapshot,
    chunker::chunk_layer, serializer::serialize_chunk,
    executor::{Executor, LogExecutor},
    storage::MemoryStore,
};
use bundler::occ::{AccountFetcher, FetchedAccount, collect_target_accounts};
use cpsr_types::intent::{AccessKind, AccountAccess, UserIntent};
use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

/// Tiny deterministic PRNG so we don't need rand crate
struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self { Self(seed) }
    fn next_u32(&mut self) -> u32 {
        // classic LCG: good enough for demo randomness
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        (self.0 >> 32) as u32
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        let i = (self.next_u32() as usize) % xs.len();
        &xs[i]
    }
    fn pick_bool(&mut self, p_true_perc: u32) -> bool {
        (self.next_u32() % 100) < p_true_perc
    }
    fn range(&mut self, lo: u32, hi: u32) -> u32 { lo + (self.next_u32() % (hi - lo + 1)) }
}

fn mk_intent(
    accounts: &[(Pubkey, bool)], // (key, writable?)
    priority: u8,
    tag: u8,
    expires_at_slot: Option<u64>,
) -> UserIntent {
    let program = Pubkey::new_unique();
    let actor = Pubkey::new_unique();

    let metas: Vec<AccountMeta> = accounts.iter()
        .map(|(k,w)| if *w { AccountMeta::new(*k, false) } else { AccountMeta::new_readonly(*k, false) })
        .collect();

    let accesses: Vec<AccountAccess> = accounts.iter()
        .map(|(k,w)| AccountAccess {
            pubkey: *k,
            access: if *w { AccessKind::Writable } else { AccessKind::ReadOnly },
        })
        .collect();

    UserIntent {
        actor,
        ix: Instruction { program_id: program, accounts: metas, data: vec![tag] },
        accesses,
        priority,
        expires_at_slot,
    }
}

/// Build a richer demo batch:
/// - 8 random accounts
/// - 10 intents mixing:
///   * single-account read
///   * single-account write
///   * two-account (one ro, one rw)
///   * occasional multi-write conflict on same account
/// - priorities in [1..=10]
/// - ~50% have expirations within [current+5 .. current+50]
fn build_demo_intents() -> (Vec<UserIntent>, Vec<Pubkey>) {
    let mut rng = Lcg::new(0xC0FFEE);
    // Create a small pool of accounts
    let mut accounts: Vec<Pubkey> = Vec::new();
    for _ in 0..8 { accounts.push(Pubkey::new_unique()); }

    let mut intents: Vec<UserIntent> = Vec::new();

    // Bias to create some conflicts: pick one "popular" account
    let hot = *rng.pick(&accounts);

    for i in 0..10 {
        let priority = rng.range(1, 10) as u8;
        let has_exp = rng.pick_bool(50);
        let expires = if has_exp {
            let base: u64 = 10_000;
            Some(base + rng.range(5, 50) as u64)
        } else { None };

        let intent = match i % 4 {
            0 => {
                // single-account READ on a random account
                let a = *rng.pick(&accounts);
                mk_intent(&[(a, false)], priority, i as u8, expires)
            }
            1 => {
                // single-account WRITE; 50% chance target the hot account to force conflicts
                let a = if rng.pick_bool(50) { hot } else { *rng.pick(&accounts) };
                mk_intent(&[(a, true)], priority, i as u8, expires)
            }
            2 => {
                // two-account: one READ (random), one WRITE (maybe hot)
                let r = *rng.pick(&accounts);
                let w = if rng.pick_bool(60) { hot } else { *rng.pick(&accounts) };
                mk_intent(&[(r, false), (w, true)], priority, i as u8, expires)
            }
            _ => {
                // two-account: both READ (no ordering needed)
                let a = *rng.pick(&accounts);
                let b = *rng.pick(&accounts);
                mk_intent(&[(a, false), (b, false)], priority, i as u8, expires)
            }
        };

        intents.push(intent);
    }

    (intents, accounts)
}

// Minimal mock RPC using your AccountFetcher trait
struct MockFetcher { inner: HashMap<Pubkey,(u64,Pubkey,Vec<u8>,u64)> }
impl MockFetcher {
    fn new() -> Self { Self { inner: HashMap::new() } }
    fn insert(&mut self, k:Pubkey,l:u64,o:Pubkey,d:Vec<u8>,s:u64){
        self.inner.insert(k,(l,o,d,s));
    }
}
impl AccountFetcher for MockFetcher {
    fn fetch_many(&self, keys:&[Pubkey]) -> Result<HashMap<Pubkey,FetchedAccount>, bundler::occ::OccError> {
        let mut out = HashMap::new();
        for k in keys {
            let (lamports, owner, data, slot) = self.inner.get(k).expect("missing").clone();
            out.insert(*k, FetchedAccount{ key:*k, lamports, owner, data, slot });
        }
        Ok(out)
    }
}

fn main() -> anyhow::Result<()> {
    let cfg = RuntimeConfig::default();
    println!("================ CPSR-Lite Runner (Demo) ================");
    println!("[CFG] poll_interval={:?} max_batch={}", cfg.poll_interval, cfg.max_batch);

    // ---- Build a richer batch of mock intents
    let (intents, account_pool) = build_demo_intents();
    let mut source = VecSource::new(intents.clone()); // clone so we can log from `intents`

    // ---- Prepare a mock fetcher with entries for ALL accounts we might touch
    // Give each account a small slot variance to make OCC min/max meaningful.
    let owner = Pubkey::new_unique();
    let mut fetcher = MockFetcher::new();
    let mut slot_seed = 100u64;
    for a in &account_pool {
        // lamports: 10..1000; data bytes length 0..7; slot within [100..105]
        let lamports = 10 + ((*a).to_bytes()[0] as u64) % 991;
        let data = vec![(*a).to_bytes()[1] % 8];
        let slot = slot_seed + ((*a).to_bytes()[2] as u64 % 6);
        fetcher.insert(*a, lamports, owner, data, slot);
    }

    let mut exec = LogExecutor;
    let mut store = MemoryStore::default();

    // ---- Log the incoming batch in detail
    println!("\n[INPUT] Fetched batch of intents (up to cfg.max_batch={}):", cfg.max_batch);
    for (idx, ui) in intents.iter().enumerate() {
        let accs: Vec<String> = ui.accesses.iter().map(|aa| {
            let mode = match aa.access { AccessKind::Writable => "W", AccessKind::ReadOnly => "R" };
            format!("{}({})", aa.pubkey, mode)
        }).collect();
        println!(
            "  - intent#{:02} prio={} expires={:?} accounts=[{}] tag={}",
            idx, ui.priority, ui.expires_at_slot, accs.join(", "), ui.ix.data.get(0).cloned().unwrap_or(0)
        );
    }

    // ---- Run single iteration (demo)
    let batch = source.fetch_batch(cfg.max_batch)?;
    if batch.is_empty() {
        println!("[RUNNER] no intents; sleeping {:?}", cfg.poll_interval);
        thread::sleep(cfg.poll_interval);
        return Ok(());
    }

    // ---- Plan: DAG build + layers
    let plan = planner::plan(batch)?;
    let edge_count: usize = plan.dag.edges.iter().map(|v| v.len()).sum();
    println!("\n[PLAN] nodes={} edges={} layers={}", plan.dag.nodes.len(), edge_count, plan.layers.len());

    // Optional: show conflict edges with cause account
    if !plan.dag.conflicts.is_empty() {
        println!("[PLAN] conflicts (caused by writers, chained by (priority DESC, index ASC)):");
        for c in &plan.dag.conflicts {
            println!("  - account={}  node{} -> node{}  kind={:?}", c.account, c.a, c.b, c.kind);
        }
    } else {
        println!("[PLAN] no conflicts (all read-only or disjoint accounts).");
    }

    // Pretty-print layer composition with account footprints
    for (li, layer) in plan.layers.iter().enumerate() {
        let mut summary: Vec<String> = Vec::new();
        for &nid in layer {
            let ui = &plan.dag.nodes[nid as usize];
            let accs: Vec<String> = ui.accesses.iter().map(|aa| {
                let mode = match aa.access { AccessKind::Writable => "W", AccessKind::ReadOnly => "R" };
                format!("{}({})", aa.pubkey, mode)
            }).collect();
            summary.push(format!("node{}: prio={} exp={:?} [{}]",
                nid, ui.priority, ui.expires_at_slot, accs.join(", ")));
        }
        println!("[LAYER {}] size={} :: {}", li, layer.len(), summary.join(" | "));
    }

    // ---- Execute per-layer: OCC -> chunk -> serialize -> submit
    for (li, layer) in plan.layers.iter().enumerate() {
        // Derive OCC targets exactly like the runtime would
        let planned: Vec<_> = layer.iter().map(|&id| plan.dag.nodes[id as usize].clone()).collect();
        let targets = collect_target_accounts(&planned, /*include_readonly=*/true);
        let uniq_targets: BTreeSet<Pubkey> = targets.iter().cloned().collect();
        println!("\n[OCC {}] targets={} :: [{}]", li, uniq_targets.len(),
            uniq_targets.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(", "));

        let snap = capture_layer_snapshot(&fetcher, &plan.dag.nodes, layer, &cfg.occ)?;
        println!("[OCC {}] min_slot={} max_slot={} versions={}", li, snap.min_slot, snap.max_slot, snap.versions.len());

        // Record snapshot (for audit in a real system we'd persist this)
        store.record_snapshot(snap);

        // Chunking (stub): 1 node per chunk
        let chunks = chunk_layer(layer);
        for (ci, chunk) in chunks.iter().enumerate() {
            let txs = serialize_chunk(&plan.dag, chunk)?;
            println!("[EXEC {}:{}] chunk contains {} tx(s) -> submitting...", li, ci, txs.len());
            exec.submit(txs)?;
        }
    }

    println!("\n================ DONE (Demo Run) ================");
    println!("What you saw:");
    println!(" - A richer input set with R/W mixes, priorities, expirations");
    println!(" - A deterministic conflict-aware plan (DAG) with layered parallelism");
    println!(" - OCC snapshots per layer (min/max slot & per-account fingerprint count)");
    println!(" - Chunk/serialize/execute flow (mock executor)");
    println!("Note: this is execution-only off-chain; on-chain settlement/proofs are next.");
    Ok(())
}
