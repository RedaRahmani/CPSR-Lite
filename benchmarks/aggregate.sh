#!/usr/bin/env bash
set -euo pipefail

for tool in jq awk sed; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Missing required tool: $tool" >&2
    exit 1
  fi
done

: "${OUT:=./benchmarks/out}"
RESULTS="./benchmarks/results.csv"

if [ ! -d "$OUT" ]; then
  echo "Benchmark output directory not found: $OUT" >&2
  exit 1
fi

declare -a logs
shopt -s nullglob
logs=("$OUT"/*.log)
shopt -u nullglob

if [ ${#logs[@]} -eq 0 ]; then
  echo "No benchmark logs found in $OUT" >&2
  exit 1
fi

tmp_file=$(mktemp)
trap 'rm -f "$tmp_file"' EXIT

echo "scenario,run,er_total_ms,er_latency_p50,er_latency_p95,base_latency_p50,base_latency_p95,fee_er,fee_base,fee_save_pct,er_success_rate,er_total_cu,base_total_cu,route_p50,route_p95,bhfa_p50,bhfa_p95" > "$tmp_file"

trim() {
  printf '%s' "$1" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//'
}

section_line() {
  local file="$1"
  local marker="$2"
  local pattern="$3"
  awk -v marker="$marker" -v pattern="$pattern" '
    index($0, marker) {in=1; next}
    /^===/ && in {exit}
    in && index($0, pattern) {print; exit}
  ' "$file"
}

value_after_colon() {
  local line="$1"
  printf '%s' "$line" | sed 's/.*://'
}

slash_values() {
  local value
  value=$(trim "$1")
  IFS='/' read -r first second <<< "$value"
  printf '%s\n' "$(trim "$first")" "$(trim "$second")"
}

for log in "${logs[@]}"; do
  base_name=$(basename "$log")
  scenario=${base_name%%__*}
  run=$(echo "$base_name" | sed -n 's/.*__run\([0-9]\+\)\.log$/\1/p')
  if [ -z "$run" ]; then
    echo "Failed to parse run number from $base_name" >&2
    exit 1
  fi

  er_json="${log}.er.json"
  base_json="${log}.base.json"
  if [ ! -s "$er_json" ]; then
    echo "Missing ER tracing JSON for $base_name" >&2
    exit 1
  fi
  if [ ! -s "$base_json" ]; then
    echo "Missing baseline tracing JSON for $base_name" >&2
    exit 1
  fi

  er_total_ms=$(jq -re 'first(.stages[] | select(.name=="rollup.execution") | .total_ms)' "$er_json")

  er_latency_line=$(section_line "$log" "=== ER RUN REPORT ===" "latency p50/p95 (ms)")
  base_latency_line=$(section_line "$log" "=== BASELINE RUN REPORT ===" "latency p50/p95 (ms)")
  er_fee_line=$(section_line "$log" "=== ER RUN REPORT ===" "total fee (lamports)")
  base_fee_line=$(section_line "$log" "=== BASELINE RUN REPORT ===" "total fee (lamports)")
  success_line=$(section_line "$log" "=== ER RUN REPORT ===" "ER success/fallback rate")
  comparison_fee_line=$(section_line "$log" "=== COMPARISON SUMMARY ===" "fee savings (lamports/% )")
  er_cu_line=$(section_line "$log" "=== COMPARISON SUMMARY ===" "ER total CU")
  base_cu_line=$(section_line "$log" "=== COMPARISON SUMMARY ===" "baseline total CU")
  route_line=$(section_line "$log" "=== ER ROUTER HEALTH ===" "route latency p50/p95 ms")
  bhfa_line=$(section_line "$log" "=== ER ROUTER HEALTH ===" "bhfa latency p50/p95 ms")

  if [ -z "$er_latency_line" ] || [ -z "$base_latency_line" ] || [ -z "$er_fee_line" ] || [ -z "$base_fee_line" ] || [ -z "$success_line" ] || [ -z "$comparison_fee_line" ] || [ -z "$er_cu_line" ] || [ -z "$base_cu_line" ] || [ -z "$route_line" ] || [ -z "$bhfa_line" ]; then
    echo "Missing required metrics in $base_name" >&2
    exit 1
  fi

  { read -r er_latency_p50 er_latency_p95; } < <(slash_values "$(value_after_colon "$er_latency_line")")
  { read -r base_latency_p50 base_latency_p95; } < <(slash_values "$(value_after_colon "$base_latency_line")")
  fee_er=$(trim "$(value_after_colon "$er_fee_line")")
  fee_base=$(trim "$(value_after_colon "$base_fee_line")")

  success_values=$(trim "$(value_after_colon "$success_line")")
  er_success_rate=$(printf '%s' "$success_values" | awk -F'/' '{print $1}' )
  er_success_rate=$(trim "$er_success_rate")

  fee_save_pct=$(printf '%s' "$comparison_fee_line" | sed -n 's/.*(\(-\{0,1\}[0-9]*\.?[0-9]*\)%).*/\1/p')
  if [ -z "$fee_save_pct" ]; then
    echo "Failed to parse fee savings percentage in $base_name" >&2
    exit 1
  fi

  er_total_cu=$(trim "$(value_after_colon "$er_cu_line")")
  base_total_cu=$(trim "$(value_after_colon "$base_cu_line")")

  { read -r route_p50 route_p95; } < <(slash_values "$(value_after_colon "$route_line")")
  { read -r bhfa_p50 bhfa_p95; } < <(slash_values "$(value_after_colon "$bhfa_line")")

  printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
    "$scenario" \
    "$run" \
    "$er_total_ms" \
    "$er_latency_p50" \
    "$er_latency_p95" \
    "$base_latency_p50" \
    "$base_latency_p95" \
    "$fee_er" \
    "$fee_base" \
    "$fee_save_pct" \
    "$er_success_rate" \
    "$er_total_cu" \
    "$base_total_cu" \
    "$route_p50" \
    "$route_p95" \
    "$bhfa_p50" \
    "$bhfa_p95" >> "$tmp_file"
done

if [ "$(wc -l < "$tmp_file")" -le 1 ]; then
  echo "No benchmark rows were parsed" >&2
  exit 1
fi

mv "$tmp_file" "$RESULTS"
trap - EXIT

echo "Wrote $RESULTS"
