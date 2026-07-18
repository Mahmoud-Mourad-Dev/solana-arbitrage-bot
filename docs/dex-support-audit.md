# DEX support audit + next-slice recommendation (S14B step 5)

Audited at tag `archive/pump-meteora-s13c` (= `93a9cff`). Standard: code
existence is NOT proof of quote correctness; only on-chain evidence counts.
Classes: `PARITY PROVEN` / `IMPLEMENTED BUT UNPROVEN` / `PARTIAL` / `MISSING`
/ `STALE-UNSAFE`.

## Venue matrix

| venue | class | what exists | what is missing |
|---|---|---|---|
| **Meteora DLMM** | **PARITY PROVEN** | full parser (`meteora_dlmm.rs`), bitmap traversal, limit-order layers, fee modes; **LIVE 6/6 exact both directions**; swap2 reconstruction + direct-call **SIM parity (WSOL→token)**; provenance checks | bitmap-extension arrays refused; Permission pairs refused; Token-2022 transfer-fee screened not modeled |
| **Pump AMM (sell)** | **PARITY PROVEN** | decoder, exact sell quote, dynamic fee-v2 decoder, reconstruction+substitution, **SIM 0 bps** | creator-pool BUY refused (rounding unresolved) — sell-only venue |
| **Raydium AMM v4** | **IMPLEMENTED BUT UNPROVEN** | `parsers::decode_raydium_v4` (752-B layout: status, fee num/den, need_take_pnl, open-orders link), effective-reserve math (vault + openbook open-orders − pnl), CPMM quote with on-chain fee fields, AmmStatus gate, Geyser feed + registry + differential harness vs TS | **zero on-chain output validation**: no real-swap comparison, no simulation parity, no account provenance validation at the S13C standard, no fixture capture; openbook open-orders totals staleness risk unquantified |
| **Orca Whirlpool** | **IMPLEMENTED BUT UNPROVEN** | `parsers::decode_whirlpool` (653-B layout), tick arrays (9988-B, 88 ticks), `tick_math::swap_exact_in` (sqrt-price Q64.64, fee ppm, crossings, coverage limit), differential harness vs TS | same as above: **no real-swap or simulation validation**; tick-crossing fee/rounding never checked against actual outputs; no provenance checks; whirlpool config/fee-tier accounts not decoded |
| **Raydium CLMM** | **MISSING** | nothing — no `CAMMCzo…` program id, no PoolState/tick-array decoder, no quote math anywhere in the repo (the `clmm` strings in `quote.rs` are a legacy parameter name) | everything: account layouts, tick arrays, `ammConfig` fee tiers, observation/oracle accounts, exact swap math, discovery, provenance, fixtures |
| token-program handling | PARTIAL | SPL + Token-2022 owners validated (observe pipeline); mint screening (transfer fee/hook/delegate) in `market_discovery` | screening wired to Pump∩Meteora discovery only |
| transfer-fee handling | PARTIAL | screened & rejected at discovery; proven-absent per traded mint | never modeled inside a quote |
| pool discovery | PARTIAL | dynamic Pump∩Meteora discovery; legacy static config for Raydium/Orca pools | no on-chain discovery for Raydium/Orca WSOL-USDC/USDT |
| simulation fixtures | PARTIAL | Pump + Meteora complete | none for Raydium/Orca |

## Candidate assessment (director's order)

1. **Raydium CLMM ↔ Raydium AMM v4** — CLMM leg is **MISSING** entirely
   (largest build: layouts + tick math + fee tiers + oracle accounts, all to
   the prove-or-stop standard). AMM v4 leg exists but is UNPROVEN. Total work:
   one full venue from scratch + one validation campaign.
2. **Raydium CLMM ↔ Orca Whirlpool** — needs the same missing CLMM build PLUS
   Whirlpool validation. Strictly more work than (1).
3. **Orca Whirlpool ↔ Meteora DLMM** — DLMM leg is already **PARITY PROVEN**
   (the only proven leg among all candidates); only the Whirlpool leg needs
   validation, and its decoder/tick-math already exist and are
   differential-tested. Smallest gap to a fully-proven route family.

## Recommended next slice (smallest, lowest-risk)

**S14B-1: Whirlpool quote validation against real on-chain swaps** (observe
-only; the exact method that took DLMM from "ported" to "6/6 LIVE EXACT" in
S4b).

Scope — one venue, one slice, no new DEX code beyond validation glue:
1. Select 2 liquid Whirlpool pools (WSOL/USDC, WSOL/USDT) discovered from
   on-chain program accounts (owner `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc`),
   not from old notes.
2. Snapshot-ring capture: single-slot (pool + tick arrays + vaults) snapshots;
   match real swaps whose pre-balances equal a snapshot; compare
   `tick_math::swap_exact_in` output to the actual on-chain output.
3. Fixture-ize ≥6 matched swaps (both directions) as committed regression
   tests, like `meteora_dlmm.rs` does.
4. Add Whirlpool provenance checks (pool owner, vault identity/owner/mint,
   tick-array ownership + start-index derivation) with typed rejects.
5. Verdict per direction: `DIRECT PARITY PROVEN` / `QUOTE MISMATCH` /
   `LAYOUT UNRESOLVED`.

Files/functions touched:
- `monitor/src/tick_math.rs` — validated, possibly corrected (fee/rounding).
- `monitor/src/parsers.rs` — `decode_whirlpool`/`decode_tick_array` validated;
  add missing fields if the 653-B layout drifted.
- NEW `monitor/src/bin/whirlpool_parity.rs` — snapshot-ring + swap matcher
  (pattern copied from the S4b DLMM harness; read-only, SafetyGate-exempt as
  observe tooling but still no send/sign).
- NEW `monitor/fixtures/whirlpool/…` — matched-swap fixtures.
- `monitor/src/observe_live.rs` — Whirlpool provenance helpers (later slice if
  preferred).

Acceptance criteria:
- ≥6 matched real swaps reproduce **exactly** (0 units) per direction, across
  ≥2 pools, incl. ≥1 multi-tick-array crossing — or a typed
  `QUOTE MISMATCH` report with the measured error distribution and STOP.
- Deterministic fixture tests committed; full fmt/clippy/test gate green.
- No executor/signing/submission code; no long observation run.

Why this is the lowest-risk path: it converts an existing, differential-tested
engine into a proven leg with ~one slice of validation work; combined with the
already-proven DLMM leg it yields the first fully-proven route family
(candidate 3) while deferring the expensive Raydium-CLMM build until evidence
justifies it. If the director still prefers Raydium-first for atomicity
reasons, the honest cost is: Raydium CLMM = full venue implementation from
zero, and AMM v4 = the same validation campaign recommended here for
Whirlpool; nothing about candidate 1 is smaller.

Economic gate (unchanged, applies to whichever family reaches observation):
multiple competitive-positive episodes; >1 independently active route;
meaningful delayed survival; ≈0.1 SOL/day+ causal value; sufficient executable
size; no single-flicker concentration; no frozen-spread dominance.
