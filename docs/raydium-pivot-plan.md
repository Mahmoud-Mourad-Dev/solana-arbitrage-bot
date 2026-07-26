# Raydium CLMM ↔ Raydium AMM v4 pivot — plan and acceptance gates

Single source of truth for pivot progress. Updated at the end of every phase.

**Strategy:** two-leg, single-token, WSOL-anchored, **event-triggered backrun**
across Raydium's own venues. `WSOL → TOKEN → WSOL`, exactly 2 hops, one atomic
transaction, Leg A / Leg B ∈ {`raydium-amm-v4`, `raydium-clmm`} in either order
on the same TOKEN/WSOL pair. Trigger is a Geyser state change on either pool,
evaluated in the same slot — not a timer.

**This is a heavily contested strategy.** The job is to build it correctly and
measure it honestly, not to make the numbers look good.

## Standing rules (every phase)

- Integer-only math on any money path; floats only in log formatting.
- **Reject over estimate.** A quote that cannot be computed exactly is not a quote.
- Never mix cached addresses with freshly fetched balances without provenance equality.
- Monitor and executor share `CostModel` — they can never disagree on profitability.
- Every new module ships unit tests; every venue integration ships a differential
  test against real on-chain behaviour.
- After each phase: `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
  update this file, commit.
- Do not modify `src/` (legacy TS). Do not weaken arming gates.
- If a phase's premise turns out to be wrong, STOP and report — do not build around it.

## Phase status

| Phase | Description | Gate | Status |
|---|---|---|---|
| 0 | Baseline + 4th arming gate + this plan | baseline green, plan committed | **DONE** |
| 0.5 | **Forensic falsification: is there a detectable backrun edge?** | evidence-backed go/no-go | **DONE — NO EVIDENCE-JUSTIFIED CLMM BUILD** |
| 1 | Raydium CLMM decoding (read-only, exact) | 3+ mainnet fixtures decode; PDAs match chain | **BLOCKED — gate 0.5 not met** |
| 2 | Exact CLMM swap math | ≥20 cases bit-exact vs chain | **BLOCKED — gate 0.5 not met** |
| 3 | Pair universe (WSOL on BOTH venues) | ≥30 pairs pass full funnel | **BLOCKED — gate 0.5 not met** |
| 4 | Event-driven route engine | deterministic replay; p99 < 1 ms/route | **BLOCKED — gate 0.5 not met** |
| 5 | Piecewise-exact sizing + real cost model | property test vs ternary; capacity respected | **BLOCKED — gate 0.5 not met** |
| 6 | Executor + on-chain program CLMM support | workspace green incl. new mollusk cases | **BLOCKED — gate 0.5 not met** |
| 7 | **VM-level confirmation** (never done before) | ≥24h confirm run, zero SVM mismatches | **BLOCKED — gate 0.5 not met** |
| 8 | Shadow submission → arm | written go/no-go with real numbers | **BLOCKED — gate 0.5 not met** |

### Phase 0 — baseline (COMPLETE)

- Baseline commit: **`eeb4b75`**, working tree clean.
- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace`: **246 passed / 0 failed / 0 ignored** ← regression baseline.
- **Fourth arming gate added** (`executor/src/config.rs`, `app.rs`): submission now
  requires `MODE=live` (marker-armed) **AND** `DRY_RUN=false` **AND**
  `ENABLE_SUBMIT=true` **AND** `ENABLE_JITO=true` **AND** `STRATEGY=raydium-dual`.
  The new gate is strictly **additive** — three tests assert it cannot relax
  `DRY_RUN`/`ENABLE_SUBMIT`, that only the exact string arms it, and that
  `MODE=observe` is never live. An old `.env` cannot arm the new path.

### Phase 0.5 — forensic falsification (INSERTED)

**Why inserted:** Phases 1–6 are ~6 phases of hard engineering *before* the
assumption most likely to be false gets tested. Two strategies have already been
archived after building first and measuring last. This phase costs ~1 day,
read-only, and can kill the thesis before any CLMM code exists.

**Premise being tested:** that a Raydium v4 ↔ CLMM backrun edge (a) exists as
repeated realized profit on-chain, and (b) is reachable from our public
RPC/Geyser infrastructure.

**Method (read-only, no new venue code):**
1. Collect landed transactions touching both a Raydium v4 pool and a CLMM pool
   for the same token in one transaction.
2. Compute **realized** profit from balance deltas only — never quotes, logs, or
   program return values. Account for fees, priority fees, visible Jito tips,
   rent, and unexplained transfers.
3. Classify strategy (`PURE_ATOMIC_ARBITRAGE` / `BACKRUN_ARBITRAGE` / `SANDWICH`
   / `LIQUIDATION` / `OTHER_MEV` / `UNCLEAR_REJECTED`) — never count a tx as
   arbitrage merely because it invokes two swap programs.
4. Measure **who wins**: signer concentration, tip share of gross, and whether
   the winning tx lands in the same slot as the victim swap.
5. Measure **dislocation decay** from public Geyser on busy v4+CLMM pairs.

**Gate 0.5 verdicts:**
- `PUBLICLY DETECTABLE` + repeated realized profit → proceed to Phase 1.
- `LIKELY PRIVATE-FLOW` / `BACKRUN REQUIRES ORDER FLOW` → **stop**, report, do
  not build CLMM.
- `NO EVIDENCE-JUSTIFIED ROUTE` → stop development on this family.

### Known prior evidence (context for the gate)

- Pump ↔ Meteora: **ARCHIVED** — 9.9 h corrected run, 1 single-poll flicker,
  0 delayed survival, ~0.066 SOL/day.
- Meteora ↔ Whirlpool: **ARCHIVED under the tested market selection** — 40
  validated routes drawn from a 1,093-token safe universe, 125 valid polls,
  **best gross edge 0 lamports** (negative before any cost).
- Both archives were economic rejections, not technical failures. The quote
  engines (DLMM live-exact 6/6, Pump fee-v2 SIM 0 bps, Whirlpool swapV2 29/29
  exact) are retained and reusable.

### Correction to the prior architecture diagnosis

The **executable** path was already event-driven, not polling:
`monitor/src/pipeline.rs` runs a Geyser stream at *processed* commitment →
`registry.apply_account_update` → `engine.mark_dirty(pool)` →
`engine.run_search(...)` synchronously in the update handler, over a precomputed
per-pool cycle index. The multi-second polling existed only in the observe/
research tools. The archived strategies died because **measured gross edge was
zero**, not because the trigger was slow — a faster trigger cannot rescue a round
trip that is already negative on a static snapshot.

### Phase 0.5 result (COMPLETE)

Full evidence: `docs/forensic-route-selection-s15a.md`. Tool:
`forensic-arb-scan` (read-only).

600 signatures scanned → 311 multi-DEX → **60 profitable atomic arbitrage**.

- `raydium-clmm+raydium-v4` (the proposed thesis): **5 profitable txs, 4 signers,
  0.0048 SOL total realized net — ranks 6th.**
- `pump-amm+raydium-v4`: **1.018 SOL total realized net, 8 txs, 7 signers** —
  213× the proposed family in the same window.
- Detectability for the CLMM family looks favourable (0 visible Jito tip on all
  5, deep block positions, 0.1% overall tip share of gross) — this is priority-fee
  competition, not a private-flow auction.

**Verdict: `NO EVIDENCE-JUSTIFIED RAYDIUM CLMM BUILD`.** The family passes
repeatability and detectability but fails on magnitude against a six-phase build
cost. Phases 1–8 stay blocked. Recommended next step: widen the forensic sample
(read-only) to test per-family persistence over hours before choosing any venue.

### Phase 0.5b — widened sample (COMPLETE)

Full evidence: `docs/forensic-widened-s15a.md`.

**3,654 signatures, two independent sampling methods** (pool-seeded/unbiased +
signer-seeded/winner-following), 460 profitable atomic arbitrages.

- `raydium-clmm+raydium-v4` did **not** improve under any method: 5 txs (Scan A),
  3 txs (Scan B), 0.011 SOL combined — 16th of 20 families in the unbiased scan.
- Raydium CLMM *is* used in profitable arbitrage (105 arbs / 1.13 SOL in Scan B)
  but paired with **Meteora DLMM or Orca Whirlpool**, never meaningfully with
  Raydium v4. The "Raydium against Raydium" premise is unsupported.
- Persistence inverted the first pass's headline: `pump-amm+raydium-v4` has the
  biggest totals but arrives in 1–2 hours inside ~6 minutes (a burst, 21 distinct
  signers in Scan A). `meteora-dlmm+orca-whirlpool` earns across **15 distinct
  hours over 53 h from 4 repeat signers, 12% top-tx concentration, zero Jito
  tips** — the only repeatable-business profile in the data.

**Verdict: `NO EVIDENCE-JUSTIFIED RAYDIUM CLMM BUILD` — confirmed and closed.**
Phases 1–8 remain blocked; recommend retiring this thesis.

**Open question for the director:** the most persistent family is the venue pair
archived in S14B-3. That archive measured 40 depth-ranked WSOL-major routes and
correctly found no quotable edge there; these operators trade different markets
(median 5 DEX instructions, 342k CU). Recommended next step is a read-only
reconstruction of those 45 transactions (exact pools/mints/hops + mint-safety
screen) before any build decision.

### Phase 0.5c — route reconstruction (COMPLETE)

Full evidence: `docs/forensic-route-recon-s15a.md`.

Reconstructed all 45 profitable `meteora-dlmm+orca-whirlpool` arbitrages.
Corrected an over-count (Meteora emits a 1-account event-log CPI after each swap
that the naive counter treated as a hop). True shape: **19 are 2-hop; 8 are
exactly `WSOL→USDC→WSOL`, 2 hops, one token** — the precise shape our Route
engine, ABI and on-chain program already express. Median 0.0025 SOL, 5 distinct
hours over 53 h, 3 signers, **zero Jito tips**, 184–363k CU, ALT in 45/45, no
flash loans. Operators run their own on-chain executors (`proVF4pM…`, `DF1ow4t…`)
— the same architecture as our `program/` crate.

**S14B-3's archive was caused by two of my own selection errors, not by the
strategy being unprofitable:**
1. `observe-xdex` ranked pools by depth and kept only the deepest
   (`Czfq3xZZ`, ts 4). The operators trade `BSddxwYW` (ts 32,896), `Esvfxt3j`
   (ts 2), `83v8iPyZ` (ts 1) — **all three were in my own S14B-1 discovery output
   and were discarded by the depth ranking**. The edge is between fee tiers of
   the same pair.
2. `mint_safety` rejects USDC (`HasMintAuthority` — Circle can mint), so the wide
   scan excluded every USDC route. USDC appears in 45/45 of these transactions.

Raydium CLMM stays closed (0.011 SOL, 8 txs across both scans); Phases 1–8 remain
blocked. Recommended next step: re-open Meteora↔Whirlpool as a **read-only**
observe slice with corrected pool selection (all fee tiers) and a major-asset
allowlist, to measure opportunity frequency, size and decay — the three things
still unknown.
