# S15A / Phase 0.5c — Route reconstruction: the S14B archive was wrong

READ-ONLY reconstruction of the 45 profitable `meteora-dlmm + orca-whirlpool`
arbitrages found in the widened sample. No venue code, no observe run, no
transaction building, no signing, no submission. Tool: `forensic-route-recon`.

**Headline: a working, repeatable, 2-hop WSOL↔USDC arbitrage between Meteora
DLMM and Orca Whirlpool exists and is being run profitably right now — and
S14B-3 archived it because of two identifiable selection errors in my own
methodology, not because the strategy is unprofitable.**

## Route shape (after correcting an over-count)

The raw scan reported 4–13 "DEX instructions" per transaction. That was inflated:
Meteora emits an **event-log CPI** with a single account after each swap, which
the naive counter treated as a hop. Filtering to real swap instructions
(`n_accounts >= 8`) gives the true shape:

| real swap hops | count |
|---|---|
| 2 | **19** |
| 3 | 20 |
| 4 | 4 |
| 5 | 2 |

| (real hops, distinct non-WSOL mints) | count |
|---|---|
| **(2, 1) ← exactly our engine's shape** | **8** |
| (2, 2) | 5 |
| (2, 3) | 6 |
| (3, 1) | 8 |
| (3, 3) | 9 |
| others | 9 |

**8 of 45 transactions are exactly `WSOL → TOKEN → WSOL`, 2 hops, one token,
across Meteora DLMM and Orca Whirlpool** — the precise shape our `Route` engine,
our 17-byte ABI, and our on-chain program already express.

## The 8 matching transactions

| metric | value |
|---|---|
| total realized net | 0.027453 SOL |
| median | **0.002535 SOL** per trade |
| range | 0.00241 – 0.00975 SOL |
| distinct hours | 5, spanning **53 hours** |
| signers | 3 (`591jWVDk` ×6, `2bLXQjWt`, `HQWuDd7p`) |
| **Jito tips** | **0 on all 8** |
| compute units | 184k – 363k (our default `CU_LIMIT` 700k is ample) |
| account keys | 34–57, **ALT used in 45/45** |
| flash loans | **0/45** — own capital, no borrowed liquidity |

The token is **USDC** in all 8. The tight, non-lottery profit distribution
(median ≈ p50 ≈ 0.0025 SOL) is the signature of a mechanical, repeatable
operation rather than opportunistic luck.

## Pools actually used

| venue | pool | tick spacing | fee |
|---|---|---|---|
| orca-whirlpool | `BSddxwYW73as…` (×6) | 32,896 | 100 ppm |
| orca-whirlpool | `Esvfxt3jMDdt…` | 2 | 200 ppm |
| orca-whirlpool | `83v8iPyZihDE…` | 1 | 100 ppm |
| meteora-dlmm | `3PyikuArxqoi…` (×4) | — | — |
| meteora-dlmm | `FbkX1h2YTs17…` (×2) | — | — |
| meteora-dlmm | `5XRqv7LCoC5F…` (×2) | — | — |

## Why S14B-3 missed it — two errors, both mine

**Error 1 — I ranked pools by depth and kept only the deepest.**
All three Orca pools above were **in my S14B-1 discovery output**. I discovered
them and then discarded them: `observe-xdex` selected the single deepest pool per
market, which is `Czfq3xZZ…` (tick spacing 4, fee 400 ppm, 245,763 SOL). The
operators trade `BSddxwYW…` (ts 32,896, 100 ppm), `Esvfxt3j…` (ts 2, 200 ppm) and
`83v8iPyZ…` (ts 1, 100 ppm). **The edge is between different fee tiers / tick
spacings of the same pair, not between the deepest pools.** Ranking by depth
systematically selected the pool where no edge exists.

**Error 2 — my mint-safety screen rejects USDC.**
`mint_safety::screen_mint` rejects any mint with a mint authority. USDC has one
(Circle can mint) and a freeze authority, so it fails with `HasMintAuthority`.
That rule is correct for memecoin rug protection and wrong for major
stablecoins. Consequence: the wide S14B-3 scan excluded **every USDC route** —
and 100% of this activity is on USDC. Of 14 distinct mints in these 45
transactions, only 5 pass the screen; USDC appears in **45/45** and fails.

Either error alone would have hidden the strategy. Both together made the
archive inevitable.

## What the archive got right, and what it did not

S14B-3 asked: *"is there quotable gross edge in the 40 deepest, safest
WSOL-major routes?"* — and answered correctly: **no**. That finding stands for
those 40 routes. It did **not** answer *"is there edge anywhere on this venue
pair?"*, and I over-generalised the result when I recommended archiving.

## Operator infrastructure (what we would compete against)

- **Custom on-chain executors**: `proVF4pMXVaYqmy4NjniPh4pqKNfMmsihgd4wdkCX3u`
  (28 txs) and `DF1ow4tspfHX9JwWJsAb9epbkA8hmpSEAtxXy1V27QBH` (8 txs). These
  operators deploy their own atomic arbitrage programs — the same architecture as
  our `program/` crate, which already exists and passes its mollusk suite.
- **Zero Jito tips across all 45.** This is priority-fee competition, not a
  private-order-flow auction — consistent with the earlier detectability finding.
- **Only 9 of 275 DEX invocations are top-level**; the rest are CPIs from those
  custom programs. Our program does exactly this.
- **No flash loans** — they trade their own inventory, as we would.

## Honest assessment of what this does and does not establish

**Established:** a 2-hop, single-token, WSOL↔USDC Meteora↔Whirlpool cycle
produces realized, repeated, multi-operator profit at a median of ~0.0025 SOL per
trade, with no Jito tips, in transaction shapes our existing engine and program
can express, on pools we already discovered.

**Not established — and I will not assume any of it:**
1. **Frequency.** 8 transactions in a 1,554-signature sample from 8 wallets is
   not a rate. I do not know how many such opportunities occur per hour, only
   that they recur across 53 hours.
2. **Whether we would win.** Three operators compete on priority fee with
   sub-slot reaction. We have never measured our own end-to-end latency.
3. **Whether our quotes would find them.** Both legs are parity-proven (DLMM
   6/6 live-exact, Whirlpool swapV2 29/29 exact), but our *discovery* has never
   been pointed at the correct pools with USDC allowed.
4. **The economics at our scale.** 0.0025 SOL × N/day must clear the ~0.1 SOL/day
   gate; that needs ~40 captured trades/day, and we do not know the capture rate.

## Recommendation

**Do not build Raydium CLMM** — that remains closed (0.011 SOL, 8 txs across
both scans).

**Do re-open Meteora ↔ Whirlpool with corrected selection**, as a read-only
observe slice — not a build:

1. Fix the two errors: enumerate **all fee tiers / tick spacings** per pair
   rather than the single deepest pool, and add a **major-asset allowlist**
   (USDC/USDT) that bypasses the memecoin rug screen while keeping it for
   unknown mints. Both are small, contained changes to existing code.
2. Point `observe-xdex` at the six pools above and run a short smoke, then a
   bounded observe run, measuring: how often a competitive-positive edge appears,
   its size, and its decay — the three unknowns above.
3. Only if that shows a viable rate does any executor work become justified.

This is the first target in this project with **realized, persistent,
multi-operator, corroborated on-chain profit in a shape we can already build**.
It deserves a properly-selected measurement before any further archive decision.

**Gate:** `cargo fmt --all --check` clean, `cargo clippy --workspace
--all-targets -D warnings` clean, `cargo test --workspace` **252 passed / 0
failed**.
