#!/usr/bin/env bash
# Live side-by-side parity validation.
#
# Runs the TypeScript monitor and the Rust monitor against the SAME Geyser
# feed + pools.json, each publishing to a distinct Redis channel, and starts
# the correlator that proves they emit identical opportunities for identical
# on-chain state. Ctrl-C stops everything and prints a final report.
#
# Requires (from your .env): GEYSER_ENDPOINT [+ GEYSER_X_TOKEN], RPC_ENDPOINT,
# a reachable REDIS_URL, and pools.json. Nothing here submits anything — the
# monitors only read chain state and publish opportunities.
#
# Usage:  bash validation/live/run-live.sh  [seconds]
#   optional positional arg = auto-stop after N seconds (default: run until Ctrl-C)

set -euo pipefail
cd "$(dirname "$0")/../.."

TS_CH="validation:ts"
RS_CH="validation:rs"
DURATION="${1:-0}"

# ── Preconditions ────────────────────────────────────────────────────────────
[ -f .env ] || { echo "ERROR: .env not found (copy .env.example and fill it)"; exit 1; }
# shellcheck disable=SC1091
set -a; . ./.env; set +a
: "${GEYSER_ENDPOINT:?set GEYSER_ENDPOINT in .env}"
: "${RPC_ENDPOINT:=https://api.mainnet-beta.solana.com}"
: "${REDIS_URL:=redis://127.0.0.1:6379}"

echo "checking Redis at $REDIS_URL …"
node -e "(async()=>{const {default:R}=await import('ioredis');const r=new R(process.env.REDIS_URL,{lazyConnect:true,maxRetriesPerRequest:1});await r.connect();await r.ping();console.log('  redis OK');r.disconnect();})().catch(e=>{console.error(e.message);process.exit(1)})" \
  || { echo "ERROR: cannot reach Redis at $REDIS_URL"; exit 1; }

echo "building TypeScript monitor …"; npm run build >/dev/null
echo "building Rust monitor (release) …"
if [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
cargo build --release -p arb-monitor >/dev/null 2>&1
RS_BIN=target/release/arb-monitor

PIDS=()
cleanup() {
  echo; echo "stopping…"
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# ── Launch both monitors on distinct channels + the correlator ───────────────
echo "starting TypeScript monitor  -> channel '$TS_CH'"
REDIS_OPPORTUNITY_CHANNEL="$TS_CH" REDIS_OPPORTUNITY_LIST="$TS_CH:list" \
  node dist/price-monitor.js > validation/live/ts-monitor.log 2>&1 &
PIDS+=($!)

echo "starting Rust monitor        -> channel '$RS_CH'"
REDIS_OPPORTUNITY_CHANNEL="$RS_CH" REDIS_OPPORTUNITY_LIST="$RS_CH:list" \
  "$RS_BIN" > validation/live/rs-monitor.log 2>&1 &
PIDS+=($!)

sleep 2
echo "starting correlator (Ctrl-C to stop)…"; echo
TS_CHANNEL="$TS_CH" RS_CHANNEL="$RS_CH" node validation/live/compare-live.mjs &
CORR_PID=$!
PIDS+=("$CORR_PID")

if [ "$DURATION" -gt 0 ]; then
  ( sleep "$DURATION"; kill -INT "$CORR_PID" 2>/dev/null || true ) &
fi
wait "$CORR_PID"
