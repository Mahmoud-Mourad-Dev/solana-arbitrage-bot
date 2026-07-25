# S15A / Phase 0.5b — Widened forensic sample: evidence and verdict

READ-ONLY. No venue code, no observe run, no transaction building, no signing,
no submission. Tool: `forensic-arb-scan` (now with signature **pagination** and a
per-family **persistence** table).

## Why widened

The first sample (600 signatures) ranked `raydium-clmm+raydium-v4` 6th with 5
profitable transactions — too thin a base rate to close the question either way.
This pass takes **3,654 signatures across two independent, deliberately
different sampling methods**, and adds the metric that separates a repeatable
business from a lucky burst: **distinct hours with profit**.

## Two independent scans (kept separate — never pooled)

| | Scan A — pool-seeded | Scan B — signer-seeded |
|---|---|---|
| seeds | 7 busy pools across Raydium CLMM/v4, Whirlpool, Meteora DLMM | 8 wallets that won profitable arbitrage in the first pass |
| bias | **unbiased** across venues; blind to who wins | **biased toward finding profit** (follows known winners) |
| signatures scanned | 2,100 | 1,554 |
| multi-DEX accepted | 1,093 | 361 |
| profitable atomic arbs | **240** | **220** |
| wall-clock window | **1.2 h** | **7,593 h** (paged deep into history) |
| visible Jito tip share of gross | 0.0% | 1.2% |

**Methodological note:** Scan A's window is only 1.2 h, so its "distinct hours"
column cannot measure persistence — it measures *breadth* (who is active right
now). Scan B pages deep into history, so it is the **persistence** evidence.
Neither alone is sufficient; agreement between them is the real signal.

## The Raydium CLMM ↔ AMM v4 thesis — retested

Transactions whose DEX set is **exactly** {raydium-v4, raydium-clmm}:

| sample | profitable txs | signers | total realized net |
|---|---|---|---|
| first pass (600 sigs) | 5 | 4 | 0.0048 SOL |
| Scan A (unbiased, 2,100) | 5 | 4 | 0.0048 SOL |
| Scan B (winner-following, 1,554) | **3** | 2 | 0.0062 SOL |

**It did not improve under any sampling method.** Even when following wallets
that demonstrably win arbitrage, the exact pair yields 3 transactions. Raydium
CLMM *is* heavily used in profitable arbitrage — 105 profitable arbs in Scan B
(1.13 SOL) — but almost always paired with **Meteora DLMM or Orca Whirlpool**,
not with Raydium v4. In Scan A it ranks **16th of 20** by realized net.

**The thesis' premise — "CLMM and CPMM on Raydium's own venues re-price against
each other and that dislocation is the edge" — is not supported by the data.**
These operators are not trading Raydium against Raydium.

## Corroborated families (profitable in BOTH independent scans)

| family | A n | B n | combined net |
|---|---|---|---|
| pump-amm + raydium-v4 | 25 | 6 | **8.503 SOL** |
| pump-bonding + raydium-v4 | 11 | 2 | 7.409 SOL |
| **meteora-dlmm + orca-whirlpool** | 19 | **45** | 1.336 SOL |
| meteora-dlmm + orca + raydium-clmm | 12 | 16 | 0.846 SOL |
| orca-whirlpool + raydium-clmm | 22 | 33 | 0.581 SOL |
| … | | | |
| **raydium-clmm + raydium-v4** | 5 | 3 | **0.011 SOL** |

## Persistence — the decisive column (Scan B)

| family | n | total SOL | distinct hours | span | signers | top-tx share |
|---|---|---|---|---|---|---|
| **meteora-dlmm + orca-whirlpool** | 45 | 0.651 | **15** | **53 h** | 4 | **12%** |
| meteora-dlmm + orca + raydium-clmm | 16 | 0.408 | 9 | 53 h | 5 | 63% |
| orca-whirlpool + raydium-clmm | 33 | 0.321 | 8 | — | 5 | 48% |
| meteora-dlmm + raydium-clmm | 19 | 0.114 | 8 | 74 h | 4 | 29% |
| pump-amm + raydium-v4 | 6 | **1.528** | **2** | **0.1 h** | 3 | 34% |
| pump-bonding + raydium-v4 | 2 | 0.605 | 1 | 0.0 h | 2 | 50% |

**This inverts the first pass's headline.** `pump-amm+raydium-v4` has the largest
totals in both scans, but its profit arrives in **1–2 distinct hours inside a
~6-minute span** — a burst around a token event, not a business. In Scan A its 25
transactions come from **21 different signers** in 0.9 h: a crowd piling into one
transient event, not an operation anyone runs continuously.

By contrast **`meteora-dlmm + orca-whirlpool`** in Scan B:
- 45 profitable transactions across **15 distinct hours spanning 53 hours**;
- **4 independent signers** (21/9/9/6 — repeat operators, not one-offs);
- only **12%** of profit in its single best transaction (tight, non-lottery
  distribution; median 0.00263 SOL, p10 0.00208 SOL);
- **zero Jito tips on all 45** — priority-fee competition, not a private auction;
- median **342k CU**, median **5 DEX instructions** per transaction.

That is the profile of a repeatable, publicly-contested operation.

## Verdict

**`NO EVIDENCE-JUSTIFIED RAYDIUM CLMM BUILD`** — confirmed and now closed.

Tripling the sample and adding a winner-following scan did not move
`raydium-clmm+raydium-v4` off the bottom of the table (0.011 SOL combined, 8
transactions, 6 signers). Building the most expensive venue integration in the
plan (decoder + `swap_internal` math + differential proof + ABI + on-chain arm +
resolver + ALTs) for this family is not justified. **Phases 1–8 of the Raydium
pivot stay blocked.** I recommend closing that thesis rather than revisiting it.

## The honest complication

The most persistent family — `meteora-dlmm + orca-whirlpool` — is **the venue
pair this project archived in S14B-3**.

That archive is not wrong, and this result does not overturn it. S14B-3 measured
**40 depth-ranked WSOL-major routes** and found zero *quotable* gross edge; that
finding stands for those 40 routes. These operators are earning on **different
markets**: median 5 DEX instructions and 342k CU indicates multi-hop routes on
tokens my strict `mint_safety` filter (no mint/freeze authority, no Token-2022)
and depth ranking deliberately excluded. In other words, S14B-3 answered "is
there edge in the safest, deepest 40 routes?" — correctly, no. It did not answer
"is there edge anywhere on this venue pair?"

**What is NOT yet established** (and must not be assumed):
1. That we could detect these specific opportunities pre-inclusion — the 342k CU
   / 5-instruction shape suggests routes more complex than our 2-hop engine.
2. That the tokens involved would pass our safety screen — they may be exactly
   the mint-authority/Token-2022 tokens we reject on purpose.
3. That we would win the race — 4 signers competing with zero tips means
   priority-fee competition we have never measured ourselves in.

## Recommendation

**Do not build Raydium CLMM.** For the next step I see two honest options, and I
lean to (1):

1. **Reconstruct the 45 `meteora-dlmm+orca-whirlpool` transactions in detail**
   (read-only, ~half a day): exact pools, exact token mints, hop count and
   ordering, and whether those mints pass `mint_safety`. That answers all three
   open questions above with evidence and costs nothing but RPC. If the routes
   turn out to be 2-hop on screenable mints, we have — for the first time in this
   project — a target with *realized, persistent, corroborated* profit and both
   quote engines already parity-proven (DLMM live-exact 6/6, Whirlpool swapV2
   29/29 exact).
2. **Stop route-hunting.** Three venue pairs have now been tested to an evidence
   standard; if the appetite for further exploration is exhausted, closing the
   programme on the basis of this evidence is a defensible decision.

**Gate:** `cargo fmt --all --check` clean, `cargo clippy --workspace
--all-targets -D warnings` clean, `cargo test --workspace` **252 passed / 0
failed**.
