# CPSR-Lite Benchmarking Suite

The `benchmarks/` directory hosts a repeatable harness around the `rollup_cli` example so we can demo and regression-test Ephemeral Rollup performance. It does not modify the CLI itself—everything here is an external wrapper.

## Prerequisites

- `cargo`, `bash`, `jq`, `awk`, `sed`
- Environment:
  - `RPC` – Solana RPC endpoint (e.g. `https://api.devnet.solana.com`)
  - `PAYER` – path to the payer keypair JSON
  - Optional: `ROUTER` (defaults to `https://devnet-router.magicblock.app/`), `ROUTER_KEY`, `RUNS`, `OUT`

## Quick Start

```bash
cargo build --release --example rollup_cli
bash benchmarks/run.sh       # runs scenarios, emits logs + tracing slices
bash benchmarks/aggregate.sh # produces benchmarks/results.csv
```

Or run the full pipeline via Make:

```bash
export RPC=...
export PAYER=...
make bench
```

## Artifacts

- `benchmarks/out/` (gitignored): raw stdout logs named `<scenario>__<timestamp>__runN.log` plus sidecars `.er.json` and `.base.json`
- `benchmarks/results.csv`: aggregated KPIs per scenario/run

`benchmarks/.gitignore` ensures these generated files stay out of version control.

## KPIs Captured

- `er_total_ms`: total rollup execution wall-clock (ms) from ER tracing
- `er_latency_p50/p95`, `base_latency_p50/p95`: settlement latency percentiles from ER vs baseline
- `fee_er`, `fee_base`, `fee_save_pct`: lamport totals plus baseline-vs-ER savings
- `er_success_rate`: ER success rate (fallbacks reflected separately)
- `er_total_cu`, `base_total_cu`: total compute units consumed by ER vs baseline
- `route_p50/p95`: Magic Router discovery latency percentiles
- `bhfa_p50/p95`: BHFA latency percentiles and cache health

These reuse the existing stdout sections printed by `rollup_cli`.

## Scenarios

| Scenario | Purpose |
|----------|---------|
| `s1_nocontention_20` | Low contention smoke: 20 memos only |
| `s2_mixed_40` | Mixed load with light contention |
| `s3_mixed_120` | Heavier mixed load to exercise chunking |
| `s4_hot_60` | Hot account stress (contended address supplied) |
| `s5_mixed_120_dp` | (Commented) same as `s3` but forces ER data plane once live |

Each scenario runs `RUNS` times (default 5) and records every stdout + tracing payload for auditability.

## Repro Tips

- Pin `RPC`, `ROUTER`, and `ROUTER_KEY` to stable endpoints for apples-to-apples comparisons
- Use a dedicated payer with sufficient balance; discard the first warm-up run if RPC latency is cold
- Adjust `RUNS` or `OUT` via environment variables when batching experiments
- Insist on clean builds (`cargo build --release --example rollup_cli`) before capturing demo artifacts
