#!/usr/bin/env bash
# Phase 2 acceptance demo: 3 headless llm-visuals --demo daemons, 3 panels.
# Usage: bash tools/demo3.sh [--window | output names...]
#   bash tools/demo3.sh                 # ONE window with the 3 panels stacked (default, safe on a desktop)
#   bash tools/demo3.sh DP-2 DP-3 HDMI-1  # fullscreen, one panel per output (3 surfaces)
#   bash tools/demo3.sh DP-2 DP-2 DP-2    # fullscreen, the 3 panels stacked on one output
# Panels without output are grouped into a single surface (see README, "Stacking").
# Esc / q in a panel quits; the daemons are stopped on exit.
set -euo pipefail
cd "$(dirname "$0")/.."

LV=../../target/release/llm-visuals
APP=target/release/llm-wayvisuals
# Always rebuild (a stale binary would show old behaviour); no-op when up to date.
(cd ../.. && cargo build --release -q)
cargo build --release -q

RUN="${XDG_RUNTIME_DIR:-/tmp}/llm-wayvisuals-demo"
mkdir -p "$RUN"
pids=()
cleanup() { for p in "${pids[@]}"; do kill -TERM "$p" 2>/dev/null || true; done; }
trap cleanup EXIT INT TERM

for i in 0 1 2; do
  "$LV" --demo --log-db off --export-json "$RUN/llmv-$i.json" </dev/null 2>"$RUN/llmv-$i.err" &
  pids+=($!)
done
sleep 1

themes=(green blue purple)
args=()
if [ "$#" -eq 0 ] || [ "$1" = "--window" ]; then
  args+=(--window)
  for i in 0 1 2; do args+=(--server "$RUN/llmv-$i.json,theme=${themes[$i]}"); done
else
  i=0
  for out in "$@"; do
    args+=(--server "$RUN/llmv-$i.json,theme=${themes[$i]},output=$out")
    i=$((i + 1)); [ "$i" -lt 3 ] || break
  done
fi

"$APP" "${args[@]}"
