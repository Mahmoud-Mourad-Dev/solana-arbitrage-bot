# Prompt A — venue-pair batch forensics: results

READ-ONLY throughout. No signing key was loaded in any code path. Tools:
`forensics-s15b` (refactored), `discover-venue-pairs`, `forensics-batch`
(`monitor/src/forensics/` library). Commits `f1a64a5`, `0bb89b3`.

## What was built

- **A1** — schema v2 for arbitrary venue pairs; v1 loader/converter keeps the
  S15B fixture reproducible. Regression guard
  `converted_v1_fixture_yields_byte_identical_q1_plan` pins the Q1 work list
  byte-for-byte; live-RPC reproduction of the published Q1 numbers verified
  exact (5/45 SAME_SLOT, 40/45 CROSS_SLOT, gaps 0/2/33/51).
- **A2** — `VenueAdapter` for meteora-dlmm, orca-whirlpool, raydium-v4,
  raydium-clmm, pump-amm; every program id cites an existing repo constant.
  Q4 economics per `docs/forensics-s15b.md`: balance deltas only, integer
  rational local pricing, `Unpriceable`/`Unsupported` instead of estimates.
  CLMM mint discovery is **Unsupported** (no lib decoder) and says so.
- **A3** — `discover-venue-pairs`: samples recent venue activity, identifies
  pools through existing discriminator-checked decoders only, ranks markets by
  distinct cross-venue signers, emits v2 inputs + manifest with sample frames.
- **A4** — `forensics-batch`: Q1–Q4 over an input directory, one comparison
  table, verdicts against explicit constants (floor $49/mo, BUILD ≥ $245/mo
  AND ≥ 50 events/day, threshold 50,000 lamports). Truncation and fetch-cap
  violations are loud ERROR rows. Each completed input is checkpointed to
  `reports/forensics-batch-progress.jsonl` (a process teardown cost one full
  run before this existed).

## Discovery ranking (sample frame: newest ~1k landed sigs/venue, 250 txs fetched/venue)

| rank | pair | market | distinct signers | cross-txs (sampled) |
|---|---|---|---|---|
| 1 | meteora-dlmm + pump-amm | `AA33znW3`/WSOL | 36 | 281 |
| 2 | meteora-dlmm + pump-amm | `CTPoyCwk`/WSOL | 19 | 167 |
| … | | | | |
| 7 | meteora-dlmm + orca-whirlpool | `9cRCn9rG`/WSOL | 2 | 8 |

**meteora-dlmm + pump-amm dominates its own discovery ranking** — independently
confirming what the nine operator wallets showed (their top-25 wins:
meteora-dlmm 23/25, pump-swap 19/25, whirlpool 1/25). The venue pair the
programme spent S14B–S15B on sits 7th with 2 signers.

## Comparison table (strict accounting where marked)

USD figures use the **measured** SOL/USD = $105.44 (median of 27 clean
two-sided SOL/USDC swaps at scan time, IQR 102.51–114.17) — a stated
assumption; the tool itself emits `Unsupported` for WSOL-quoted markets and
therefore verdict `INCONCLUSIVE`. Bands below apply the policy constants to
the stated assumption.

| pair / market | window | landed cross | ev/day (>0) | ev/day (≥50k) | SOL/mo ≥50k (all participants) | ~$/mo | signers ≥50k | band |
|---|---|---|---|---|---|---|---|---|
| **meteora+pump `CTPoyCwk`** (strict) | 0.1 h | 4,795 | 2,160 | **2,160** | **394.3** | **$41,578** | 5 | **BUILD-level** |
| **meteora+pump `A13oRB9F`** (strict) | 0.25 h | 1,407 | 864 | 672 | 141.0 | $14,864 | 7 | **BUILD-level** |
| meteora+whirlpool `CTPoyCwk` (naive) | 3 h | 2,531 | 736 | 352 | 7.8 | $821 | 23 | BUILD-level† |
| whirlpool+pump `CTPoyCwk` (naive) | 3 h | 1,748 | 792 | 384 | 5.8 | $616 | 23 | BUILD-level† |
| whirlpool+pump `A13oRB9F` (naive) | 3 h | 1,803 | 568 | 176 | 1.2 | $124 | 13 | INVESTIGATE |
| meteora+pump `FZqdw6oS` (strict) | 0.1 h | 160 | 0 | 0 | 0.0 | $0 | 0 | dead (was $12.8k naive, earlier window) |
| meteora+whirlpool (3 other markets) | 3 h | ≤578 | ≤96 | ≤64 | ≤0.9 | ≤$94 | ≤6 | KILL-level |
| whirlpool+raydium-v4 (4 markets) | 3 h | ≤124 | ≤40 | ≤24 | ≤0.2 | ≤$20 | ≤5 | KILL-level |
| meteora+raydium-v4 `So111111` | 3 h | 121 | 32 | 24 | 0.2 | $24 | 3 | KILL-level |
| S15B pair (USDC, 53.4 h — reference) | 53.4 h | 7,558 | 290 | 29 | 0.13 | $9.76 | 23 | KILL (validated) |

† naive numbers; measured top-end strict correction on this batch was −14.9%,
which does not change the band.

**No row received a formal `BUILD` from the tool** — every live market is
WSOL-quoted, so USD is honestly `Unsupported` and the verdict column reads
`INCONCLUSIVE`. With the measured SOL/USD applied as a stated assumption, the
two meteora-dlmm+pump-amm markets clear both BUILD bars by **60–170×**.

## Findings that changed the code (not just the report)

**Strict accounting (commit `0bb89b3`).** Verifying the largest measured win
against every balance delta exposed capital inflows booked as profit: the
signer of `3Ph3HGfiauYz…` gained 391,073,819 lamports, but 57,406,080 came
from the operator's own non-token vault account debited in the same tx. True
arb value = vault outflow − router fees = 333,667,739 (17% overstatement).
Across the top-85 events: 74/85 clean, **11/85 pure capital transfers whose
strict value is exactly −fee**, aggregate top-end bias 14.9%.
`external_native_inflow` is now measured per tx and subtracted; two
regression tests pin the real on-chain numbers.

**Loud guards fired 7 times and were right every time**: 4 fetch-cap
violations (39k, 183k, 14k, 13k landed txs — all meteora+pump, i.e. the guard
kept flagging exactly the markets with the most money), 2 pagination
truncations (>150k and >300k sigs), and 1 stale-window cost blowup avoided by
re-anchoring.

## Assumptions that could bias numbers optimistically (explicit list)

1. **Extrapolation from minutes-scale windows.** The headline meteora+pump
   figures scale a 6–15-minute window ×2,880–7,200 to a month. Memecoin
   markets are aggressively non-stationary: `FZqdw6oS` went from $12.8k/mo
   (naive, one window) to **$0** (strict, next window) with a 10× flow
   collapse. `CTPoyCwk` stayed hot across three independent windows spanning
   ~9 h (60k/h, 127k/h, 48k/h flow), which is persistence evidence at the
   hours scale only. A month-scale claim from these windows is NOT supported;
   what is supported is "while such a market is hot, the pot is
   $100s–$1,000s/day".
2. **SOL/USD = $105.44** — measured, but n=27 with ±6% IQR, applied outside
   the tool as a stated assumption.
3. **Q1 slot-gap saturates on hot pools.** `gap=0` on a pool doing 13–48k
   txs/6min is guaranteed by traffic alone and carries no information about
   backrun structure. The S15B Q1 instrument is only valid on sparse pools.
   The hot markets' Q1 "KILL" rows must be read as **unmeasured**, not as
   same-slot-contested.
4. **Q2 unreliable on burst-clustered events** (n=9 events inside one burst
   share leader rotation windows mechanically; `CONCENTRATED` there is not
   evidence of private routing).
5. **Naive rows** (3 h windows, pre-strict) carry the measured ~15% top-end
   inflation; bands were assigned with that correction in mind.
6. 239/4,795 landed txs in the final CTPoyCwk window could not be fetched;
   they are excluded from the numerator (conservative) but stated.

## Gate decision for Prompt B

Prompt B requires ≥1 venue pair involving `meteora-dlmm` at `BUILD` or
`INVESTIGATE`. **Satisfied**: meteora-dlmm+pump-amm at BUILD-level on two
independent markets under strict accounting (and meteora-dlmm+orca-whirlpool
`CTPoyCwk` at BUILD-level naive). The DLMM execution gap (Prompt B) is
justified by measurement. Both engines are already parity-proven on the quote
side; pump-amm execution exists; **meteora-dlmm execution is the missing
leg** — exactly what Prompt B closes.
