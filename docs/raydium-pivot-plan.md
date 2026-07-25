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
| 0.5 | **Forensic falsification: is there a detectable backrun edge?** | evidence-backed go/no-go | **IN PROGRESS** |
| 1 | Raydium CLMM decoding (read-only, exact) | 3+ mainnet fixtures decode; PDAs match chain | blocked on 0.5 |
| 2 | Exact CLMM swap math | ≥20 cases bit-exact vs chain | blocked on 0.5 |
| 3 | Pair universe (WSOL on BOTH venues) | ≥30 pairs pass full funnel | blocked on 0.5 |
| 4 | Event-driven route engine | deterministic replay; p99 < 1 ms/route | blocked on 0.5 |
| 5 | Piecewise-exact sizing + real cost model | property test vs ternary; capacity respected | blocked on 0.5 |
| 6 | Executor + on-chain program CLMM support | workspace green incl. new mollusk cases | blocked on 0.5 |
| 7 | **VM-level confirmation** (never done before) | ≥24h confirm run, zero SVM mismatches | blocked on 0.5 |
| 8 | Shadow submission → arm | written go/no-go with real numbers | blocked on 0.5 |

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
