# S14B-2 — Meteora DLMM ↔ Orca Whirlpool cross-DEX discovery

Quote-only, read-only observe slice. Cycle: **WSOL → token on Meteora DLMM →
WSOL on Orca Whirlpool swapV2**, token ∈ {USDC, USDT}. NO executor / tx
construction / keypair / signing / two-leg simulation / Jito / submission /
deployment / S10-S11 / live. Legacy Whirlpool `swap-v1` is never used.

## Architecture

- **Shared route engine** (`route_engine.rs`): new `Leg::Whirlpool`
  (`WhirlpoolLegState`) quotes token→WSOL with the proven `swap_exact_in` math
  (swapV2 semantics — variant-agnostic quoting). leg1 = `Leg::Meteora`
  (WSOL→token). Discovery, optimizer, and reporting all call this one engine.
- **Single-tick clamp** (`tick_math::single_tick_capacity` +
  `WhirlpoolLegState::single_tick_boundary`): the Whirlpool leg computes the
  nearest initialized-tick boundary in the swap direction and quotes with an
  empty crossing set limited to that boundary. Any size that would cross is a
  typed **capacity** reject (`SingleTickExceeded { capacity }`) — the optimizer
  treats it exactly like DLMM bin exhaustion (search smaller), so it can NEVER
  explore or quote an unproven crossing size. This is enforced structurally, not
  by a post-hoc quote failure.
- **Provenance** (`whirlpool_parity` + inline Meteora checks): pool owner,
  decoded mints == market, vault identity/owner/mint/authority, tick-array
  PDA+start+back-pointer, classic-SPL-only (Token-2022 rejected). Meteora pair:
  DLMM owner, sides == {WSOL, token}, reserves == decoded. Typed rejects; no
  cached relationship is combined with fresh balances without an equality check.
- **Snapshot**: per leg, decode fresh + validate + hash (sha256 of pool/pair
  account), record both slots. Bin arrays ±2 around active (Meteora); 5 tick
  arrays around current tick (Whirlpool).
- **Events / episodes**: emits `narrow_report::PollEvent` (so `aggregate_narrow`
  + `rebuild-report` work unchanged) with a new `xdex: XdexProvenance` field
  (pair, pool, direction, size, token_mid, wsol_out, meteora_fee, whirlpool_fee,
  current tick, single-tick capacity, `no_tick_crossed`, both slots, both
  hashes). Every attempt writes exactly one event; failures carry
  `valid_snapshot=false` + typed reason. Reuses the S13C-corrected episode
  pipeline (gaps split, invalid can't extend, failed reconfirm ≠ survival).

## Discovery result (on-chain, deepest per market)

| market | Meteora DLMM pair | Whirlpool pool (ts) |
|---|---|---|
| WSOL/USDC | `5rCf1DM8…` | `Czfq3xZZ…` (ts 4) |
| WSOL/USDT | `ANeTpNwP…` | `FwewVm8u…` (ts 2) |

Both venues validated to share exactly {WSOL, quote} classic-SPL mints.

## Smoke run (this session, ~50 s)

- sweeps 7, events 14, **ok 14**, **clamp-confirmed-no-cross 14/14**.
- Single-tick capacity is real and binding (e.g. WSOL/USDC ≈ 7.26 SOL max
  within-tick input) — recorded in every event.
- Candidates before competitive costs: **0** (no size produced positive gross).
- Candidates after competitive costs: **0**. Episodes: **0**. Both routes
  classed `NeverProfitable`. Causal detect/day: **0 lamports**.
- `offline == live` metrics: **true** (rebuild-report equivalence, manifest-
  driven, no flags).

Interpretation: the machinery runs end-to-end and is honest; WSOL/USDC and
WSOL/USDT across two efficient major venues showed no competitive-positive edge
in this short window, and the single-tick clamp additionally caps executable
size. A short smoke cannot establish (or refute) a daily edge.

## Tests added

Route engine: within-tick quote + crossing clamp, optimizer clamps below a
crossing, wrong-mint reject, real-DLMM-leg1 + Whirlpool-leg2 composition.
`narrow_report`: xdex provenance survives JSONL roundtrip and aggregates
identically. Whirlpool provenance (vault/mint/tick-array/Token-2022) covered in
`whirlpool_parity` + `whirlpool_fixture_tests`. Full gate: fmt, clippy
`--workspace -D warnings`, `cargo test --workspace` 238 passed / 0 failed.

## VPS command for a later 10–12 h observe run

```bash
cargo build --release -p arb-monitor --bin observe-xdex --bin rebuild-report
mkdir -p reports/xdex
RPC_ENDPOINT="<helius-url>" XDEX_INTERVAL_SECS=5 XDEX_MAX_SOL=5 \
XDEX_OUT_DIR=reports/xdex \
nohup ./target/release/observe-xdex --capture 43200 \
  > reports/xdex/run.log 2>&1 &          # 43200 s = 12 h
echo $! > reports/xdex/run.pid
# status:  tail -f reports/xdex/run.log
# safe stop (graceful): kill -INT "$(cat reports/xdex/run.pid)"
# export + independent offline check (manifest-driven, no flags):
./target/release/rebuild-report reports/xdex/xdex-*.jsonl
tar -czf xdex-12h-$(date +%s).tar.gz reports/xdex/
```

## Recommendation

A longer economic-validation run is **only marginally justified**. The single-
tick clamp binds executable size on these deep pools, and WSOL/USDC-USDT are the
most competed pairs on Solana — a persistent capturable edge is a priori
unlikely, and the same ~0.1 SOL/day gate that archived Pump↔Meteora applies. If
the director wants a definitive read, run the 10–12 h observe above and decide
on the corrected causal metrics; do NOT authorize atomic simulation on local
quote parity alone. A higher-expected-value alternative is to widen discovery to
more volatile shared markets (still observe-only) before committing VPS time to
the two efficient stablecoin pairs.

Stop point: no long run, no atomic simulation started.
