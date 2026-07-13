# Implementation Plan (Phase 0 → Live)

## Progress snapshot (2026-07-13)

Done and committed (each fmt+clippy-clean, tests green; ~119 tests):
- **S1 modes** — observe/replay/simulate/live, default observe; live gated by
  flag + `.live-armed` marker.
- **S2 shared cost model** — one integer `CostModel` used by monitor AND
  executor (kills the split-tip defect).
- **S3 Pump AMM** — parser + quote verified vs mainnet. SELL exact for ALL
  pools; BUY exact for creator-less pools; creator-pool BUY refused (rounding
  unresolved). Fee model pinned from on-chain swap EVENTS.
- **S4 + S4b Meteora DLMM** — parsers + exact Q64.64 price (140/140 bins) +
  faithful port of Meteora's official quote (limit orders, collect-fee-mode,
  bitmap traversal). **Live parity 6/6 EXACT, both directions.**
- **S5 discovery** — dynamic Pump∩Meteora scan + Token-2022 mint-safety
  screen + ranked restart cache. Live: 430k+110k pools → 9,975 both-venue →
  ~205 above a 2-SOL floor → 203 safe.
- **S6 route engine** — two-leg WSOL→token→WSOL, both directions, typed
  reject taxonomy, shared cost model.
- **S7 optimizer** — two-stage grid+ternary; beats brute force; capacity-aware.
- **S9 (essence)** — live simulation-parity proven for both quote engines.
- **S12 + S12b observe-markets** — live scanner, single-slot-consistent per
  market, full reject taxonomy, JSON reports. **Never submits.**

Pending: S8 (formal replay harness — essence covered live), S10 (on-chain
`execute_*` + intermediate-minimum check, sim-only), S11 (tx builder
hardening), S13 (24–72h observe run → gates), S14 (small-cap live — BLOCKED on
explicit approval). Known precision gap: creator-pool Pump BUY rounding (limits
pump-first routes; ~98% of pools are creator pools, usable as the SELL leg).

Runbook:
```
cargo run -p arb-monitor --release --bin discover-markets      # refresh cache
cargo run -p arb-monitor --release --bin observe-markets -- --once   # one scan
cargo run -p arb-monitor --release --bin observe-markets       # loop (OBS_INTERVAL_SECS)
```

---

Ordered, small, verifiable stages. **Default mode stays `observe`; live stays
disabled** until every acceptance gate passes and you explicitly approve.
Each stage: explain → smallest change → fmt → clippy → unit → integration →
show failures honestly → do not advance on red.

Guiding rules (non-negotiable): integer-only financials; never fabricate
liquidity/bins/quotes; local quote must satisfy `local_out ≤ simulated_out`
within a documented tolerance; missing data → structured error, never a fake
profit.

---

## Stage ordering & dependencies

```
S1 modes+config ─► S2 shared cost model ─► S3 Pump parser+quote ─┐
                                          ─► S4 Meteora parser+quote ─┤
S5 dynamic discovery (Pump∩Meteora) ◄──────────────────────────────┘
S6 two-leg route engine ─► S7 two-stage optimizer
S8 replay harness ─► S9 simulation-parity harness   (validate S3/S4 math)
S10 on-chain execute_* + intermediate check (sim only)
S11 tx builder v0/ALT/precache + account/size/compute checks (sim only)
S12 observe reports + failure classifier + metrics
S13 24–72h observe run ─► GATES ─► S14 small-cap live (only on approval)
S15 (Stage 2) Orca pairs
```

---

## S1 — Four explicit modes + config scaffolding
- Add `Mode { Observe, Replay, Simulate, Live }`, default `Observe`. Central
  `BotConfig` (integer lamports throughout) with the strategy scope, freshness
  limits, risk caps, `execution_payment` config, and per-mode guards.
- `Live` requires an explicit flag **and** an acceptance marker file; refuse
  otherwise.
- Files: new `common` or `monitor::config` additions; `.env.example`; a new
  `config.example.toml`.
- Verify: unit tests that `Live` is unreachable without both flag+marker; default
  parses to `Observe`.

## S2 — Shared cost model + `execution_payment`
- New `arb_common::cost` (integer): components (sig, CU×price, priority, ATA,
  rent, relay/Jito, margin, required_net) and
  `max_payment = gross − non_payment_costs − required_net − margin`; reject ≤0.
- `execution_payment` trait: `JitoTip`, `PrivateRelay`, `NoPayment`.
- **Both** monitor route-eval and executor consume this exact model (kills the
  split defect).
- Verify: unit tests incl. "monitor net == executor net for same inputs";
  property test `max_payment` never eats required_net.

## S3 — Exact Pump AMM support (verify before trust)
- Parser (pool + config), vault/fee resolution, direction handling, **integer**
  exact quote, swap-data + CPI account order.
- Verify layouts against: official program/IDL if available, real on-chain
  account dumps, and (in S9) `simulateTransaction`. Do **not** claim exact until
  S9 parity passes.
- Tests: quote monotonicity, both directions, fee correctness, no-overestimate,
  layout decode vs real account fixtures.

## S4 — Exact Meteora DLMM support (verify before trust)
- LB-pair parser (active bin, bin step, fee params, variable fee), bin-array
  discovery + parser (+ bitmap/extension), integer bin traversal, remaining-
  accounts order, swap builder.
- Missing bins → `InsufficientBinCoverage` (structured); never a partial quote.
- Tests: one-bin, multi-bin, exact boundary crossing, insufficient bins, zero
  liquidity, both directions, fee calc, rounding, local-vs-sim placeholder
  (filled in S9).

## S5 — Dynamic Pump∩Meteora discovery
- Discover tokens with valid liquidity on **both** Pump AMM and Meteora DLMM;
  detect migrated Pump tokens; resolve all accounts; validate ownership/status/
  token-program; Token-2022 allow-safe / reject-unsafe extensions; frozen/malicious
  mint checks; dedup; in-memory graph; **persistent restart cache**; activity-based
  ranking (swaps/reserve-or-bin changes/divergence, not just TVL); eviction.
- Tests: cache round-trip; dedup; unsafe-extension rejection; ranking ordering.

## S6 — Two-leg route engine (Pump ↔ Meteora)
- Route A: Pump WSOL→token, Meteora token→WSOL. Route B: mirror.
- Full per-route economics via S2 cost model; rejects on stale/owner/mint/bins/
  tx-size/compute/account-count.
- Reuse discovery substrate but constrain to exactly 2 pools / WSOL-anchored.
- Tests: both directions; reject taxonomy; near-miss surfacing.

## S7 — Two-stage integer optimizer
- Coarse grid (configurable ladder, capped by risk/WSOL/pool-safe/tx-safe) →
  keep top-3 → local integer refine. Objective = **max projected net**.
- Per-size logging. Replaces ternary in the new path (keep ternary for the old
  path until retired).
- Tests: recovers known optimum on synthetic concave curve; respects caps.

## S8 — Replay harness
- Deterministic replay from recorded account states / real signatures. Answers:
  would we discover the token, build the route, pick what size, predict gross/net,
  pass filters; local-vs-actual output delta; required/missing accounts.
- **Independently verify program IDs + layouts**; do not trust forensic labels.
- Output: `reports/replay/<sig>.{json,md}`.

## S9 — Simulation-parity harness (the trust gate for S3/S4)
- Build the real tx, call `simulateTransaction`, compare local vs simulated leg
  outputs, compute units, net; record parity error (bps + raw).
- **Blocks live eligibility** until parity proven across a meaningful set.

## S10 — On-chain `execute_pump_to_meteora` / `execute_meteora_to_pump`
- Record base → CPI leg1 → **check intermediate minimum** → CPI leg2 → require
  `final ≥ start + min_profit` or revert. Strict validation for Pump+Meteora
  program IDs, pool owners, mint/vault relationships, signer/writable, dup
  defense, token program. Simulation-only until gates pass.
- Tests: mollusk — both legs in sim, intermediate-min enforced, unprofitable
  reverts atomically, account-validation negative cases.

## S11 — Transaction builder hardening
- v0 + ALT + compute budget + priority + optional `execution_payment` + atomic
  ix + blockhash + **tx-size & account-count & compute pre-validation**. Pre-create
  ATAs/ALTs off the hot path; cache pool/token/ALT/program/static accounts.

## S12 — Observe reporting + failure classifier + metrics
- Observe never sends. Rich reports (`reports/observe-<ts>.{json,md}`): pools/
  tokens tracked, evaluations, gross/net-positive, executable, full rejection
  taxonomy, best near-miss/spread/net/size, opportunity lifetime.
- Failure classifier → `reports/failures.json` + summary. Metrics surface
  (latencies, counts, parity error, category failures).

## S13 — 24–72h observe run → acceptance gates
- Run observe on live mainnet; produce the Gate-7 report. Decide with evidence.

## S14 — Small-cap live (ONLY on explicit approval)
- Tiny max size, daily-loss cap, failed/consecutive-failure caps, min net,
  max age/skew, min confidence, kill switch, balance reserve, no auto size
  increase. Full confirmed-profit accounting; stop on reconciliation mismatch.

## S15 — Stage 2: Orca Whirlpool pairs
- Pump↔Orca, Meteora↔Orca. Reuse existing exact tick-array math + dynamic
  tick-array subscriptions. Only after Stage-1 gates pass.

---

## Acceptance gates (mirror of the spec)
1. Discovery dynamic (no static-4 reliance), ownership/mint verified, cache
   survives restart.
2. Exact quote math: Pump & Meteora local == simulation within documented
   rounding; no overestimation; missing bins → explicit error.
3. Route engine: both directions, multi-size, near-misses, one shared cost
   model, no monitor/executor mismatch.
4. Replay: multiple known target-strategy txs replayed; correct 2-leg route;
   predicted ≈ actual; mismatches explained.
5. Atomic execution: both CPIs sim-work; on-chain min-profit + intermediate
   check; unprofitable reverts; validation tests pass.
6. Tx readiness: v0+ALT, measured compute, size within limits, ATAs pre-created.
7. Observe 24–72h report with economics + latency.
8. Live readiness: explicit approval + all safety controls + confirmed accounting.

## Known risks / unknowns to resolve early
- Pump AMM & Meteora DLMM **exact layouts and swap formulas** must be confirmed
  against IDL + real accounts + simulation (S3/S4/S9) — highest technical risk.
- Meteora **variable fee** and bin-array **bitmap/extension** correctness.
- Token-2022 transfer-fee/hook math inside quotes (must be integer-exact or
  reject).
- Pump "migrated token" detection semantics.
- Whether a **generic forwarder** program suffices or dedicated Pump/Meteora
  instructions are safer (account-validation strictness).

## What I am NOT doing without your go-ahead
- No live submission. No program deploy. No Geyser purchase. No deletion of the
  working Raydium/Orca path until Pump/Meteora is validated.
