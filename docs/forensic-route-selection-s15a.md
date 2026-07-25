# S15A / Phase 0.5 — Forensic route selection: evidence and verdict

READ-ONLY forensic scan of landed mainnet transactions. No new venue code, no
observe run, no transaction building, no signing, no submission. Tool:
`monitor/src/bin/forensic_arb_scan.rs` (`forensic-arb-scan`).

**Purpose:** test the load-bearing premise of the Raydium CLMM ↔ AMM v4 backrun
thesis *before* spending 6 phases building a CLMM venue — is there repeated
realized net profit in that family, and is it reachable from public RPC?

## Method

- Sample successful signatures from busy seed pools (Raydium CLMM SOL/USDC,
  Raydium AMM v4 SOL/USDC, +2 more), fetch each with `jsonParsed`.
- Collect DEX programs from **outer AND inner (CPI)** instructions; map to a
  stable family label. Jupiter is excluded from the family key (it is an
  aggregator, not an implementable venue pair).
- **Realized profit from balance deltas only** — never a quote, log claim, or
  program return value:
  `net = signer SOL delta + signer WSOL token-account delta`
  (the fee and any Jito tip are already outflows inside the SOL delta),
  `gross = net + fee + visible Jito tip`.
- Visible Jito tips detected as SOL increases on the 8 known mainnet tip accounts.
- Classification is structural, never "it touched two swap programs":
  `PURE_ATOMIC_ARBITRAGE` requires ≥2 DEX programs **AND** a closed round trip
  **AND** net > 0. Everything else is `OTHER_MEV` / `UNCLEAR_REJECTED`.
  Three unit tests pin this (single-DEX is never arbitrage; no closed loop is
  never arbitrage; negative net is never profitable arbitrage).

## Dataset

| | |
|---|---|
| signatures scanned | **600** (4 seed pools, deduplicated, successful only) |
| multi-DEX accepted | **311** |
| `PURE_ATOMIC_ARBITRAGE` (closed loop, net > 0) | **60** |
| `OTHER_MEV` | 251 |
| rejected (typed) | 0 in the accepted path — non-multi-DEX txs are skipped, not rejected |

Accounting validated by inspection: the large negative "net" rows are ordinary
swaps/snipes (`wsol_delta = 0`, signer spending SOL to acquire tokens), correctly
classified `OTHER_MEV` — not arbitrage losses.

## Route-family ranking (profitable atomic arbitrage only)

| family | n | total net (SOL) | median (lamports) | p90 | signers |
|---|---|---|---|---|---|
| **pump-amm + raydium-v4** | 8 | **1.018111** | 869,080 | 512,292,562 | 7 |
| meteora-dlmm + orca-whirlpool + raydium-clmm | 4 | 0.258859 | 7,946 | 258,842,499 | 4 |
| orca-whirlpool + raydium-clmm | 5 | 0.051118 | 1,046,732 | 40,785,575 | 4 |
| orca-whirlpool + raydium-clmm + raydium-v4 | 2 | 0.033653 | 16,826,415 | 33,650,127 | 2 |
| meteora-dlmm + raydium-clmm + cpmm + v4 + zerofi | 2 | 0.026836 | 13,417,761 | 13,420,079 | 2 |
| **raydium-clmm + raydium-v4** ← *the proposed thesis* | **5** | **0.004779** | **94,554** | 2,675,521 | 4 |
| meteora-dlmm + raydium-clmm | 11 | 0.000889 | 8,681 | 100,801 | 5 |
| raydium-clmm + raydium-cpmm | 6 | 0.000200 | 8,877 | 107,273 | 4 |
| (12 further families, all < 0.002 SOL total) | | | | | |

## The proposed thesis, isolated

Transactions whose DEX set is **exactly {raydium-v4, raydium-clmm}**: **7 total**,
of which **5** are profitable atomic arbitrage.

| sig | class | net (lamports) | tip | signer |
|---|---|---|---|---|
| `4Pu1eekxqk4n` | PURE_ATOMIC_ARBITRAGE | 2,675,521 | 0 | `591jWVDk` |
| `42uQp8FDBjHd` | PURE_ATOMIC_ARBITRAGE | 1,914,581 | 0 | `2bLXQjWt` |
| `5bHxRc7oJs9J` | PURE_ATOMIC_ARBITRAGE | 94,554 | 0 | `R32xAccF` |
| `4G7YGRA8b56G` | PURE_ATOMIC_ARBITRAGE | 76,648 | 0 | `HuTshmtw` |
| `5H2B5RbZXPhh` | PURE_ATOMIC_ARBITRAGE | 17,688 | 0 | `HuTshmtw` |
| `4AJsZRgShdF1` | OTHER_MEV | −20,001 | 0 | `7UZrRu3n` |
| `619BQwmW6bCW` | OTHER_MEV | −20,001 | 0 | `9tokHbm2` |

**Total realized net across the whole sample: 0.00478 SOL.** Median 94,554
lamports (≈0.0000945 SOL). Four distinct signers — no single dominant operator.

## Answering the three required questions

**1. Was it profitable on-chain?** Yes — but marginally, and it is **not** the
leading family. `raydium-clmm+raydium-v4` ranks **6th** by realized net, at
**0.0048 SOL** total versus **1.018 SOL** for `pump-amm+raydium-v4` — a **213×**
difference in the same sample window. The thesis' premise ("Raydium's own venues
re-price at different rates, and that's where the money is") is **not supported**:
the money in this sample is overwhelmingly in pump-amm ↔ raydium-v4, i.e. exactly
the *volatile new-token* venue class, not the deep stable Raydium pair.

**2. Could our monitor have detected it before inclusion?** Partly — and this is
the encouraging part. Tip share of visible gross was **0.1%**, and the five
profitable v4↔CLMM arbs paid **zero visible Jito tip**. Block-position analysis of
the top earners shows they land deep in the block (indices 170–1376 of 1149–1573),
not in the protected leading bundles. That is *not* the signature of a
private-order-flow auction; it looks like ordinary priority-fee competition.
So: `PUBLICLY DETECTABLE` for this family — but see the caveat below.

**3. Could our infrastructure realistically execute it?** Unproven. Detectability
is necessary but not sufficient; nothing here measures whether we would *win* the
race, and 5 events in a 600-signature window is a very thin base rate.

## Verdict

**`NO EVIDENCE-JUSTIFIED RAYDIUM CLMM BUILD`**

The evidence gate (Phase 8 of the pivot prompt) requires: repeated profitable
transactions on the current CLMM program, reconstructed counterpart DEX, exact
pools/mints, realized positive net after fees/tips, persistence beyond an isolated
event, and a plausible detection path. The family **passes on repeatability
(5 events, 4 signers) and detectability (no tip, deep block position)** but
**fails on magnitude**: 0.0048 SOL total realized net in the sample, ranking 6th,
against a build cost of six engineering phases (full CLMM decoder + exact
`swap_internal` math + differential proof + ABI change + on-chain program arm +
executor resolver + ALTs). Building the most expensive venue for the 6th-best
family is not justified by this evidence.

**What the evidence *does* point at:** `pump-amm + raydium-v4` — 1.018 SOL
realized net, 8 events, 7 distinct signers, and **both venues' quote engines
already exist and are parity-proven in this repo** (Pump AMM sell: SIM 0 bps with
the decoded dynamic fee-v2; Raydium v4: implemented, though only
differential-tested). That is a fraction of the build cost of CLMM.

**Honest caveats on that redirection:** (a) the pump-amm leg is the same venue
class this project already archived on economics — the difference is that this
sample shows *realized landed profit* rather than quoted spreads, which is
stronger evidence than we ever had before; (b) 8 events is still a thin base rate;
(c) the p90 (0.51 SOL) is dominated by a few large events with 4,000,000-lamport
tips, meaning that family's tail **is** tip-competitive even if the median is not;
(d) Pump creator-pool BUY remains unproven in our quote engine, and a
`pump-amm + raydium-v4` cycle would likely need it.

## Recommendation

**Do not build Raydium CLMM on this evidence.** Two defensible options:

1. **Widen the forensic sample first** (cheap, read-only, ~1 day): scan several
   thousand signatures across more seed pools and a longer window, and measure
   per-family *persistence across hours*, not just totals. If
   `raydium-clmm+raydium-v4` stays 6th, the question is closed permanently. This
   is the option I recommend.
2. **Pivot the target family** to `pump-amm + raydium-v4` and run the same
   evidence gate against it before writing code — including whether the winning
   transactions are backruns we could see, and whether they require the unproven
   creator-BUY path.

Phases 1–8 of the Raydium pivot remain **blocked** pending that evidence.

**Gate:** `cargo fmt --all --check` clean, `cargo clippy --workspace
--all-targets -D warnings` clean, `cargo test --workspace` **252 passed / 0
failed** (+3 forensic classification tests).
