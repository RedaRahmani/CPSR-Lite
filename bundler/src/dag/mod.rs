//! DAG builder for CPSR-Lite.
//!
//! DAG (Directed Acyclic Graph) builder — it figures out the order of transactions so none of them fight over the same account.
//! If two people want to change the same pool at the same time, one must go first.
//! Where it’s used: Before chunking, so we know the safe execution order.
//!
//! Given a list of `UserIntent`s (each with a precomputed access list), this module:
//! - Detects read/write conflicts on accounts
//! - Imposes a deterministic total order over conflicting intents
//! - Constructs a conflict-free DAG (edges encode "must run before")
//! - Provides topological sorting with cycle detection
//! - Provides "layers" of concurrently-safe nodes for planning or simulation
//!
//! Design notes
//! ------------
//! * We use a *deterministic total order* to resolve any potential scheduling ambiguity.
//!   The order key is: (priority DESC, original_index ASC). This yields stable plans.
//! * For each account, we gather all intents that reference it. If any are Writable,
//!   we sort those participating intents by the order key and create a chain of edges
//!   along that order (n_i -> n_{i+1}). This enforces a safe sequencing for W/W and
//!   R/W hazards, while allowing pure R/R sets to remain unconstrained.
//! * The union across all accounts yields the overall DAG.
//! * Even though edges follow a total order, we still run a generic topological sort
//!   with cycle detection as a correctness guard (and for flexibility if you change
//!   ordering policy later).
//!
//! Complexity
//! ----------
//! Let N = number of intents, A = total referenced accounts across all intents.
//! Building the per-account map: O(A). For each account with k participants, we sort
//! them: O(k log k), and add O(k) edges. Summed over all accounts, this is acceptable
//! for typical bundle sizes (tens to low hundreds of ops).
//!
//! Safety
//! ------
//! - No on-chain dependencies here. Pure planning logic.
//! - Deterministic: same inputs -> same DAG, regardless of HashMap iteration order.
//!
//! Unit tests at the bottom cover basic hazards and layer extraction.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
/// HashSet is a data structure that stores unique items — no duplicates allowed
use cpsr_types::{intent::{AccessKind, AccountAccess, UserIntent}};
use solana_program::pubkey::Pubkey;

/// Node identifier inside the DAG (index into Dag::nodes)
pub type NodeId = u32;

/// What kind of hazard led to an ordering edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// A write after write or write after read hazard exists on an account.
    /// (Any participation of a writer on that account triggers ordering.)
    WriteInvolved,
    /// Pure read/read (no ordering required) – not emitted as an edge.
    ReadOnlyOnly,
}

/// One concrete conflict explanation (useful for diagnostics / metrics).
#[derive(Debug, Clone)]
pub struct Conflict {
    pub account: Pubkey,
    pub a: NodeId,
    pub b: NodeId,
    pub kind: ConflictKind,
}

/// A directed acyclic graph representing ordering constraints between intents.
#[derive(Debug, Clone)]
pub struct Dag {
    /// Original intents, in input order.
    pub nodes: Vec<UserIntent>,
    /// Adjacency list: edges[u] contains v for each edge u -> v.
    pub edges: Vec<Vec<NodeId>>,
    /// Reverse adjacency for fast indegree/layer computations.
    pub rev_edges: Vec<Vec<NodeId>>,
    /// Optional conflict explanations (deduplicated).
    pub conflicts: Vec<Conflict>,
    /// Mapping from (u,v) to ensure we don't emit duplicate edges.
    edge_set: HashSet<(NodeId, NodeId)>,
}

/// Errors that can arise when building/processing the DAG.
#[derive(Debug, thiserror::Error)]
pub enum DagError {
    #[error("cycle detected in DAG (this should not happen with total-order chaining)")]
    CycleDetected,
    #[error("index out of bounds")]
    IndexOob,
}

impl Dag {
    /// Build a conflict-free DAG from a list of intents.
    ///
    /// Deterministic ordering key: (priority DESC, original_index ASC).
    pub fn build(intents: Vec<UserIntent>) -> Self {
        let n = intents.len();
        let mut dag = Dag {
            nodes: intents,
            edges: vec![Vec::new(); n],
            rev_edges: vec![Vec::new(); n],
            conflicts: Vec::new(),
            edge_set: HashSet::new(),
        };

        // Map: account pubkey -> participants touching it (with their rw mode and stable order key)
        #[derive(Clone)]
        struct Participant {
            node: NodeId,
            access: AccessKind,
            // Sorting key for deterministic sequencing
            // priority desc (larger first), then original index asc (smaller first)
            sort_key: (i32, u32),
        }

        let mut by_account: HashMap<Pubkey, Vec<Participant>> = HashMap::new();

        // Pre-compute sort keys & fill by_account map
        for (idx, intent) in dag.nodes.iter().enumerate() {
            let node_id = idx as NodeId;
            let key = (-(intent.priority as i32), idx as u32); // DESC priority, ASC index
            for AccountAccess { pubkey, access } in &intent.accesses {
                by_account
                    .entry(*pubkey)
                    .or_default()
                    .push(Participant { node: node_id, access: *access, sort_key: key });
            }
        }

        // For each account, impose a chain over participants when a writer is involved.
        let mut conflict_buf: Vec<Conflict> = Vec::new();
        for (account, mut parts) in by_account {
            // Fast path: if all are read-only, skip (no ordering needed).
            if parts.iter().all(|p| matches!(p.access, AccessKind::ReadOnly)) {
                continue;
            }
            // Sort by deterministic key: priority desc, index asc.
            parts.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));

            // Create chain edges u -> v along the sorted list.
            for pair in parts.windows(2) {
                let u = pair[0].node;
                let v = pair[1].node;
                // Avoid self-loops and duplicates
                if u != v && dag.edge_set.insert((u, v)) {
                    dag.edges[u as usize].push(v);
                    dag.rev_edges[v as usize].push(u);
                    conflict_buf.push(Conflict {
                        account,
                        a: u,
                        b: v,
                        kind: ConflictKind::WriteInvolved,
                    });
                }
            }
        }

        // Deduplicate conflicts by (account, a, b)
        let mut uniq = HashSet::new();
        for c in conflict_buf {
            if uniq.insert((c.account, c.a, c.b)) {
                dag.conflicts.push(c);
            }
        }

        dag
    }

    /// Kahn's algorithm for topological sort.
    /// Returns a vector of NodeIds in a valid execution order.
    pub fn topo_order(&self) -> Result<Vec<NodeId>, DagError> {
        let n = self.nodes.len();
        let mut indeg = vec![0u32; n];
        for v in 0..n {
            for &u in &self.rev_edges[v] {
                indeg[v] = indeg[v].saturating_add(1);
            }
        }

        // Use a stable queue seeded by increasing node index to keep determinism
        let mut q: VecDeque<NodeId> = VecDeque::new();
        for v in 0..n {
            if indeg[v] == 0 {
                q.push_back(v as NodeId);
            }
        }

        let mut out = Vec::with_capacity(n);
        while let Some(u) = q.pop_front() {
            out.push(u);
            for &v in &self.edges[u as usize] {
                let dv = &mut indeg[v as usize];
                *dv = dv.saturating_sub(1);
                if *dv == 0 {
                    q.push_back(v);
                }
            }
        }

        if out.len() != n {
            return Err(DagError::CycleDetected);
        }
        Ok(out)
    }

    /// Partition nodes into "layers" where each layer contains nodes whose current indegree is zero.
    /// Running layer-by-layer respects dependencies; within a layer, nodes are independent w.r.t. the built constraints.
    pub fn layers(&self) -> Result<Vec<Vec<NodeId>>, DagError> {
        let n = self.nodes.len();
        let mut indeg = vec![0u32; n];
        for v in 0..n {
            for &u in &self.rev_edges[v] {
                indeg[v] = indeg[v].saturating_add(1);
            }
        }
        let mut layers: Vec<Vec<NodeId>> = Vec::new();
        let mut frontier: BTreeSet<NodeId> = BTreeSet::new(); // BTreeSet for deterministic within-layer ordering

        for v in 0..n {
            if indeg[v] == 0 {
                frontier.insert(v as NodeId);
            }
        }

        let mut remaining = n;
        while !frontier.is_empty() {
            let mut layer: Vec<NodeId> = Vec::with_capacity(frontier.len());
            // Consume current frontier in ascending NodeId order
            for v in frontier.iter().copied() {
                layer.push(v);
            }
            // Prepare next frontier
            let mut next_frontier: BTreeSet<NodeId> = BTreeSet::new();
            for &u in &layer {
                for &v in &self.edges[u as usize] {
                    let dv = &mut indeg[v as usize];
                    *dv = dv.saturating_sub(1);
                    if *dv == 0 {
                        next_frontier.insert(v);
                    }
                }
            }
            layers.push(layer);
            remaining -= layers.last().unwrap().len();
            frontier = next_frontier;
        }

        if remaining != 0 {
            return Err(DagError::CycleDetected);
        }
        Ok(layers)
    }

    /// Return all outgoing neighbors of node `u`.
    pub fn neighbors(&self, u: NodeId) -> Result<&[NodeId], DagError> {
        self.edges.get(u as usize).map(|v| v.as_slice()).ok_or(DagError::IndexOob)
    }

    /// Return all incoming neighbors of node `v`.
    pub fn predecessors(&self, v: NodeId) -> Result<&[NodeId], DagError> {
        self.rev_edges.get(v as usize).map(|v| v.as_slice()).ok_or(DagError::IndexOob)
    }
}

// -------------------------------
// Unit tests
// -------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use solana_program::{instruction::{AccountMeta, Instruction}, pubkey::Pubkey};

    fn mk_intent(
        actor: Pubkey,
        program: Pubkey,
        accounts: &[(Pubkey, bool /*writable*/)],
        data: Vec<u8>,
        priority: u8,
    ) -> UserIntent {
        let metas: Vec<AccountMeta> = accounts.iter()
            .map(|(k, w)| if *w { AccountMeta::new(*k, false) } else { AccountMeta::new_readonly(*k, false) })
            .collect();
        let accesses = accounts.iter().map(|(k, w)| {
            let access = if *w { AccessKind::Writable } else { AccessKind::ReadOnly };
            AccountAccess { pubkey: *k, access }
        }).collect();

        UserIntent {
            actor,
            ix: Instruction { program_id: program, accounts: metas, data },
            accesses,
            priority,
            expires_at_slot: None,
        }
    }

    #[test]
    fn read_read_same_account_no_edge() {
        let a = Pubkey::new_unique();
        let prog = Pubkey::new_unique();
        let actor = Pubkey::new_unique();
        let i0 = mk_intent(actor, prog, &[(a, false)], vec![0], 5);
        let i1 = mk_intent(actor, prog, &[(a, false)], vec![1], 4);
        let dag = Dag::build(vec![i0, i1]);
        // R/R => no edges
        assert!(dag.edges.iter().all(|e| e.is_empty()));
        assert_eq!(dag.topo_order().unwrap().len(), 2);
        assert_eq!(dag.layers().unwrap().len(), 1); // both can be in same layer
    }

    #[test]
    fn write_write_forces_chain_by_priority_desc_then_index() {
        let acc = Pubkey::new_unique();
        let prog = Pubkey::new_unique();
        let actor = Pubkey::new_unique();

        // Three writers with differing priorities
        let i0 = mk_intent(actor, prog, &[(acc, true)], vec![0], 1); // low priority
        let i1 = mk_intent(actor, prog, &[(acc, true)], vec![1], 9); // high priority (should come first)
        let i2 = mk_intent(actor, prog, &[(acc, true)], vec![2], 5); // mid

        let dag = Dag::build(vec![i0, i1, i2]);
        // order key: priority DESC -> i1, i2, i0
        let order = dag.topo_order().unwrap();
        // topological order respects edges; since edges chain by that key, sequence must follow
        // We cannot assert exact positions because other accounts could add edges, but here it's single account only.
        let pos = |nid: NodeId| order.iter().position(|&x| x == nid).unwrap();
        assert!(pos(1) < pos(2) && pos(2) < pos(0)); // i1 before i2 before i0
    }

    #[test]
    fn read_then_write_chain_enforced() {
        let a = Pubkey::new_unique();
        let prog = Pubkey::new_unique();
        let actor = Pubkey::new_unique();

        // i0 reads a (prio 5), i1 writes a (prio 4) -> both participate; chain by order key (i0 before i1)
        let i0 = mk_intent(actor, prog, &[(a, false)], vec![0], 5);
        let i1 = mk_intent(actor, prog, &[(a, true)],  vec![1], 4);

        let dag = Dag::build(vec![i0, i1]);
        let order = dag.topo_order().unwrap();
        let pos = |nid: NodeId| order.iter().position(|&x| x == nid).unwrap();
        assert!(pos(0) < pos(1));
    }

    #[test]
    fn layers_group_independent_nodes() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let prog = Pubkey::new_unique();
        let actor = Pubkey::new_unique();

        // i0 writes a, i1 writes b, i2 writes a
        let i0 = mk_intent(actor, prog, &[(a, true)], vec![], 5);
        let i1 = mk_intent(actor, prog, &[(b, true)], vec![], 5);
        let i2 = mk_intent(actor, prog, &[(a, true)], vec![], 4);

        let dag = Dag::build(vec![i0, i1, i2]);
        let layers = dag.layers().unwrap();

        // Example expected layering:
        // layer0 could contain the highest-priority writers across distinct accounts: likely i0 and i1
        // layer1 then contains i2 because it conflicts with i0 on account `a`.
        assert!(layers.len() >= 2);
        let l0: BTreeSet<NodeId> = layers[0].iter().copied().collect();
        // Either i0 or i1 may appear in layer0 (both ideally), but i2 must be in a later layer.
        assert!(l0.contains(&0));
        assert!(l0.contains(&1));
        assert!(layers.iter().flatten().copied().any(|n| n == 2));
    }
}
