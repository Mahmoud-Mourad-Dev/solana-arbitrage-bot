#!/usr/bin/env bash
# Phase B validation driver: runs the TypeScript and Rust discovery engines
# over the SAME fixtures (scenarios.json) and asserts their emitted
# opportunities are semantically identical. Fully offline — no Geyser/Redis.
#
# Usage: bash validation/run-parity.sh
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== building TypeScript =="
npm run build --silent

echo "== TS engine =="
npx tsx validation/ts-harness.ts

echo "== Rust engine =="
if [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
cargo run --quiet -p arb-monitor --bin parity_harness

echo "== diff =="
node validation/compare.mjs
