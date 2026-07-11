# Gap Analysis (Phase 0)

Target strategy: **atomic 2-pool arbitrage, WSOL → token → WSOL, across
Pump AMM ↔ Meteora DLMM** (Orca as second stage). This document is the honest
delta between what exists and what the target requires. Nothing below is
implemented yet unless marked ✅.

Legend: ✅ exists & reusable · 🟡 exists but must change · ❌ missing (build) ·
⚠️ defect to fix.

## A. Market coverage — the core gap

| Need | Status |
|---|---|
| Pump AMM pool parser | ❌ missing |
| Pump AMM exact integer quote | ❌ missing |
| Pump AMM swap instruction builder + account order | ❌ missing |
| Meteora DLMM LB-pair parser | ❌ missing |
| Meteora DLMM bin-array parser + traversal | ❌ missing |
| Meteora DLMM exact integer quote (variable fee, bins) | ❌ missing |
| Meteora DLMM swap instruction + remaining accounts | ❌ missing |
| Raydium v4 / Orca Whirlpool | ✅ (Orca to be reused in Stage 2; Raydium out of scope) |

This is ~70% of the new work. The **exact-CLMM pattern in `tick_math.rs`** (step
through initialized ticks, cross `liquidity_net`, reject on missing coverage,
round against the trade) is the direct template for **DLMM bin traversal** —
bins are the analogue of ticks. So the *shape* is proven; the *math and layouts*
are new and must be verified against real accounts + simulation before trust.

## B. Pool discovery

| Need | Status |
|---|---|
| Dynamic discovery of tokens on BOTH Pump AMM & Meteora DLMM | ❌ (today: Raydium/Orca HTTP APIs → static file) |
| Detect migrated Pump tokens | ❌ |
| Resolve mint / WSOL side / program / vaults / fees / aux accounts | ❌ (per-DEX) |
| Ownership / status / token-program validation | 🟡 (bootstrap does size+mint checks for Raydium/Orca; not Pump/Meteora) |
| Token-2022 support + reject unsafe transfer extensions | ❌ |
| Frozen / malicious mint detection | ❌ |
| Dedup, in-memory graph, restart cache | 🟡 (in-memory graph ✅ via registry/adjacency; dedup ✅; **no persistent cache**) |
| Rank by activity/divergence (not just TVL) | ⚠️ today ranks by 24h volume; need swap-rate / reserve-change / cross-DEX divergence |
| Inactive-pool eviction | ❌ |

## C. Route engine

| Need | Status |
|---|---|
| Two-leg WSOL→token→WSOL, both directions | 🟡 substrate exists (`max_hops=2`, base WSOL) but hard-wired to Raydium/Orca quotes |
| Per-route economics (gross, fees, tx cost, priority, relay/Jito, margin, net, age, freshness, confidence) | 🟡 partial — has gross/cost/net/slot; **no quote confidence, no slot-skew, no tx-size/compute pre-check** |
| Reject on stale / wrong-owner / unsupported mint / missing bins / size limits / compute too high | 🟡 partial (freshness ✅, missing-ticks ✅); tx-size & compute pre-checks ❌ |

## D. Optimizer

| Need | Status |
|---|---|
| Two-stage integer optimizer (coarse grid + local refine, keep top-3) | ❌ today: ternary only |
| Objective = max **net** after all costs | 🟡 evaluate uses net for the gate but ternary optimizes gross-minus-input, then subtracts fixed cost after |
| Per-size logging (in, leg1, final, gross, costs, net, reject reason) | ❌ not per-size |

## E. Cost model ⚠️ (defect flagged in prior audit)

| Need | Status |
|---|---|
| ONE shared cost model (monitor = executor) | ⚠️ **split** — monitor fixed tip, executor dynamic tip; they disagree |
| Components: sig, CU, CU-price, priority, ATA, rent, relay/Jito, margin, required net | 🟡 scattered; not unified |
| `execution_payment` abstraction (Jito tip / private relay / none) | ❌ today hard-codes "Jito tip" |
| `max_payment = gross − non_payment_costs − required_net − margin`, reject if ≤0 | ❌ |

## F. On-chain executor

| Need | Status |
|---|---|
| Atomic 2-leg, record base → CPI → check intermediate → CPI → require net or revert | ✅ **pattern exists** (generic forwarder + on-chain profit check) |
| `execute_pump_to_meteora` / `execute_meteora_to_pump` | 🟡 can be the generic forwarder with 2 new `DexKind`s, OR dedicated instructions; needs Pump/Meteora swap-data builders + program-id constants + account validation |
| Strict validation (program IDs, pool owners, mint/vault relationships, signer/writable, dup defense, token program) | 🟡 has program-id + owner + signer checks; **mint/vault-relationship checks are DEX-specific and missing for Pump/Meteora** |
| Intermediate-minimum check between legs | ❌ current program checks only final base balance, not the intermediate token minimum |
| Pre-created ATAs/ALTs, no hot-path account creation | 🟡 executor ensures ATAs lazily (hot path!); ALT supported |

## G. Transaction builder

| Need | Status |
|---|---|
| v0 tx, ALT, compute budget, priority fee, payment, atomic swap ix, blockhash mgmt, size est, account-count validation | 🟡 most ✅; **explicit tx-size/account-count validation before send** partial (1232 check ✅, account-count ❌); payment is Jito-only |
| Build before opportunity is stale; cache pool meta / token accounts / ALT / program accounts / static ix accounts | 🟡 resolver caches Raydium/Whirlpool keys; **no Pump/Meteora caches** |

## H. State ingestion

| Need | Status |
|---|---|
| Staged feed: RPC bootstrap, WS subscribe, replay | 🟡 RPC ✅, Geyser ✅; **WS account-subscribe ❌, replay ❌** |
| Pluggable low-latency feed (Geyser / ShredStream) not coupled to strategy | 🟡 Geyser exists but strategy is coupled to the poll/stream shape |
| Per-update metadata: slot, write-version, pubkey, owner, lamports, data, source, recv ts, decode ts | ⚠️ only slot (+ per-chunk in preview). **Rest missing** |
| Per-opportunity: newest/oldest slot, slot skew, age µs, source latency | 🟡 has newest slot only; **skew/age-µs/latency missing** |

## I. Modes & harnesses

| Need | Status |
|---|---|
| Explicit `observe / replay / simulate / live`, default observe | ❌ today: `DRY_RUN`/`ENABLE_SUBMIT`/`ENABLE_JITO` flags; `preview`≈observe |
| Observe reports (JSON+MD) with rich rejection taxonomy & near-misses | 🟡 preview writes JSON + printed report; **MD ❌, taxonomy partial** |
| Replay harness (fixtures from real signatures, verify route/size/quote vs actual) | ❌ missing |
| Simulation-parity harness (local vs `simulateTransaction`, bps error) | 🟡 executor can simulate; **no systematic parity harness / dataset** |
| Failure classifier + `reports/failures.json` | ❌ |
| Metrics (latencies, counts, parity error, category failures) | 🟡 preview has some counters; no metrics surface |

## J. Financial-correctness invariants (already satisfied — keep)

- ✅ Integer-only math (U256/U512); no float in quotes/costs. **Keep this bar for Pump/Meteora.**
- ✅ Never overestimate output (conservative rounding + reject on missing data). **Must hold for DLMM bins: return `InsufficientBinCoverage`, never a partial fake quote.**
- ✅ On-chain profit-or-revert. **Keep; add intermediate-minimum check.**
- ✅ Consistency guards (slot spread / freshness / sleep-gap / single-slot confirm).

## Reuse summary

**Reuse as-is or lightly:** `common` ABI (extend), registry+consistency guards,
`tick_math` *pattern*, executor builder/ALT/blockhash/Jito, Pinocchio program
*pattern*, mollusk test harness, preview loop skeleton, Orca Whirlpool (Stage 2).

**Replace/build:** Pump AMM + Meteora DLMM parsers/quotes/instructions,
dynamic Pump∩Meteora discovery, two-stage optimizer, shared cost model +
`execution_payment`, 4 explicit modes, replay + simulation-parity harnesses,
richer state metadata, failure classifier + metrics.

**Delete/retire (only after replacement proven):** Raydium-v4-specific discovery
ranking and quote in the *primary* path (keep code/tests until Pump/Meteora
validated). Nothing is deleted in Phase 0.
