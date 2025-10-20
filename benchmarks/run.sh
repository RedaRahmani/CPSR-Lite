#!/usr/bin/env bash
set -euo pipefail

for tool in awk sed; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Missing required tool: $tool" >&2
    exit 1
  fi
done

: "${OUT:=./benchmarks/out}"
: "${RUNS:=5}"
: "${ROUTER:=https://devnet-router.magicblock.app/}"
: "${RPC?Set RPC to the Solana RPC endpoint (e.g. https://api.devnet.solana.com)}"
: "${PAYER?Set PAYER to the path of the payer keypair JSON}"

BIN="${BIN:-./target/release/examples/rollup_cli}"

mkdir -p "$OUT"

if [ ! -x "$BIN" ]; then
  echo "Building rollup_cli example (release)..."
  cargo build --release --example rollup_cli
fi

stable_flags=(
  --er-enabled
  --er-router "$ROUTER"
  --er-min-cu-threshold 0
  --er-merge-small-intents
  --er-route-ttl-ms 60000
  --er-http-timeout-ms 3000
  --er-connect-timeout-ms 1000
  --er-blockhash-ttl-ms 10000
)

extract_json_section() {
  local label="$1"
  local input="$2"
  local output="$3"

  awk -v label="=== TRACING REPORT JSON (""${label}"") ===" '
    $0 == label {capture=1; next}
    capture && /^=== / {exit}
    capture {print}
  ' "$input" > "$output"

  if [ ! -s "$output" ]; then
    echo "Failed to extract tracing report JSON for ${label} from $(basename "$input")" >&2
    exit 1
  fi
}

run_case() {
  local scenario="$1"
  shift
  local -a extras=("$@")

  echo "=== Scenario ${scenario} (RUNS=${RUNS}) ==="

  for run in $(seq 1 "$RUNS"); do
    local ts
    ts=$(date -u +"%Y%m%dT%H%M%S.%3NZ")
    local log_file="$OUT/${scenario}__${ts}__run${run}.log"

    local -a cmd=(
      "$BIN"
      --rpc "$RPC"
      --payer "$PAYER"
      --mode compare
    )
    cmd+=("${stable_flags[@]}")
    if [ -n "${ROUTER_KEY:-}" ]; then
      cmd+=(--er-router-key "$ROUTER_KEY")
    fi
    cmd+=("${extras[@]}")

  local command_line
  command_line=$(printf '%q ' "${cmd[@]}")
  printf '+ %s\n' "$command_line"
  printf '+ %s\n' "$command_line" > "$log_file"

  "${cmd[@]}" | tee -a "$log_file"
    local status=${PIPESTATUS[0]}
    if [ "$status" -ne 0 ]; then
      echo "Command failed for ${scenario} run ${run}" >&2
      exit "$status"
    fi

    extract_json_section "ER" "$log_file" "${log_file}.er.json"
    extract_json_section "Baseline" "$log_file" "${log_file}.base.json"

    echo "Wrote ${log_file}" >&2
  done
}

run_case s1_nocontention_20 \
  --demo-mixed \
  --demo-conflicting-to hkRHSurdP4gsZfmpTT3dE39LMw5W28YJQMJGWR545GD \
  --mixed-memos 20 \
  --mixed-contended 0

run_case s2_mixed_40 \
  --demo-mixed \
  --demo-conflicting-to hkRHSurdP4gsZfmpTT3dE39LMw5W28YJQMJGWR545GD \
  --mixed-memos 40 \
  --mixed-contended 1

run_case s3_mixed_120 \
  --demo-mixed \
  --demo-conflicting-to hkRHSurdP4gsZfmpTT3dE39LMw5W28YJQMJGWR545GD \
  --mixed-memos 80 \
  --mixed-contended 4

run_case s4_hot_60 \
  --demo-mixed \
  --demo-conflicting-to hkRHSurdP4gsZfmpTT3dE39LMw5W28YJQMJGWR545GD \
  --mixed-memos 40 \
  --mixed-contended 6

# Future data-plane scenario (requires live ER data plane)
# run_case s5_mixed_120_dp \
#   --demo-mixed \
#   --mixed-memos 80 \
#   --mixed-contended 4 \
#   --er-endpoint https://devnet-us.magicblock.app \
#   --er-require
